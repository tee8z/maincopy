use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize, de};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LogicalContentPath(String);

impl LogicalContentPath {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalContentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct PublicationSource<'source> {
    path: LogicalContentPath,
    contents: &'source str,
}

impl<'source> PublicationSource<'source> {
    pub fn new(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
        }
    }

    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn contents(&self) -> &'source str {
        self.contents
    }
}

#[derive(Clone, Debug)]
pub struct PostSource<'source> {
    path: LogicalContentPath,
    contents: &'source str,
}

impl<'source> PostSource<'source> {
    pub fn new(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
        }
    }

    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn contents(&self) -> &'source str {
        self.contents
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PostId {
    value: Uuid,
    canonical: String,
}

impl PostId {
    pub fn parse(value: &str) -> Result<Self, PostIdParseError> {
        let parsed = Uuid::parse_str(value).map_err(|_| PostIdParseError)?;
        if parsed.hyphenated().to_string() != value {
            return Err(PostIdParseError);
        }
        Ok(Self {
            value: parsed,
            canonical: value.to_owned(),
        })
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.value
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

impl fmt::Display for PostId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for PostId {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PostId {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("post ID must be a canonical lowercase hyphenated UUID")]
pub struct PostIdParseError;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlainTextError {
    #[error("value must not be empty")]
    Empty,
    #[error("value must not contain control characters")]
    ContainsControl,
}

fn normalize_plain_text(value: impl Into<String>) -> Result<String, PlainTextError> {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(PlainTextError::Empty);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(PlainTextError::ContainsControl);
    }
    Ok(trimmed.to_owned())
}

macro_rules! plain_text_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, PlainTextError> {
                normalize_plain_text(value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

plain_text_type!(SiteTitle);
plain_text_type!(SiteDescription);
plain_text_type!(AuthorName);
plain_text_type!(PostTitle);
plain_text_type!(PostDescription);
plain_text_type!(PrivacyPolicyRevision);
plain_text_type!(DistributionCopy);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("value must use lowercase ASCII words separated by single hyphens")]
pub struct RouteValueError;

fn is_route_safe(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

macro_rules! route_value_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, RouteValueError> {
                let value = value.into();
                if is_route_safe(&value) {
                    Ok(Self(value))
                } else {
                    Err(RouteValueError)
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

route_value_type!(PostSlug);
route_value_type!(PostAlias);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PostTag(String);

impl PostTag {
    pub fn parse(value: impl Into<String>) -> Result<Self, RouteValueError> {
        let normalized = value.into().trim().to_ascii_lowercase();
        if is_route_safe(&normalized) {
            Ok(Self(normalized))
        } else {
            Err(RouteValueError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationBaseUrl(Url);

impl PublicationBaseUrl {
    pub fn parse(value: &str) -> Result<Self, PublicationBaseUrlError> {
        let value = value.trim();
        let has_valid_raw_authority = value.split_once("://").is_some_and(|(_, remainder)| {
            let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
            let authority = &remainder[..authority_end];
            let suffix = &remainder[authority_end..];
            !authority.contains('@') && matches!(suffix, "" | "/")
        });
        let mut parsed = Url::parse(value).map_err(|_| PublicationBaseUrlError)?;
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || !has_valid_raw_authority
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path() != "/"
        {
            return Err(PublicationBaseUrlError);
        }
        parsed.set_path("/");
        Ok(Self(parsed))
    }

    pub fn as_url(&self) -> &Url {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Serialize for PublicationBaseUrl {
    fn serialize<Serializer>(
        &self,
        serializer: Serializer,
    ) -> Result<Serializer::Ok, Serializer::Error>
    where
        Serializer: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("base URL must be an absolute HTTPS origin without credentials, path, query, or fragment")]
pub struct PublicationBaseUrlError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct UnresolvedAssetReference(String);

impl UnresolvedAssetReference {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, PlainTextError> {
        normalize_plain_text(value).map(Self)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by asset resolution in WP 1.6")
    )]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct UnresolvedHttpsOrigin(String);

impl UnresolvedHttpsOrigin {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, PlainTextError> {
        normalize_plain_text(value).map(Self)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by asset resolution in WP 1.6")
    )]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MarkdownSource(String);

impl MarkdownSource {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Publishable,
    Draft,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostTipPolicy {
    InheritPublication,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionMode {
    Disabled,
    Enabled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XDistributionSettings {
    mode: DistributionMode,
    copy: Option<DistributionCopy>,
}

impl XDistributionSettings {
    pub(crate) const fn new(mode: DistributionMode, copy: Option<DistributionCopy>) -> Self {
        Self { mode, copy }
    }

    pub const fn mode(&self) -> DistributionMode {
        self.mode
    }

    pub const fn copy(&self) -> Option<&DistributionCopy> {
        self.copy.as_ref()
    }
}

impl Default for XDistributionSettings {
    fn default() -> Self {
        Self::new(DistributionMode::Disabled, None)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DistributionSettings {
    x: XDistributionSettings,
}

impl DistributionSettings {
    pub(crate) const fn new(x: XDistributionSettings) -> Self {
        Self { x }
    }

    pub const fn x(&self) -> &XDistributionSettings {
        &self.x
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownDialect {
    CommonMark,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawHtmlPolicy {
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeRenderingMode {
    EscapedPlainText,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MermaidRenderingMode {
    Placeholder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RendererSettings {
    markdown: MarkdownDialect,
    raw_html: RawHtmlPolicy,
    code: CodeRenderingMode,
    mermaid: MermaidRenderingMode,
}

impl RendererSettings {
    pub const fn baseline() -> Self {
        Self {
            markdown: MarkdownDialect::CommonMark,
            raw_html: RawHtmlPolicy::Disabled,
            code: CodeRenderingMode::EscapedPlainText,
            mermaid: MermaidRenderingMode::Placeholder,
        }
    }

    pub const fn markdown(&self) -> MarkdownDialect {
        self.markdown
    }

    pub const fn raw_html(&self) -> RawHtmlPolicy {
        self.raw_html
    }

    pub const fn code(&self) -> CodeRenderingMode {
        self.code
    }

    pub const fn mermaid(&self) -> MermaidRenderingMode {
        self.mermaid
    }
}

impl Default for RendererSettings {
    fn default() -> Self {
        Self::baseline()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TipAmount(NonZeroU64);

impl TipAmount {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TipAmountRange {
    minimum: TipAmount,
    maximum: TipAmount,
}

impl TipAmountRange {
    pub fn new(minimum: TipAmount, maximum: TipAmount) -> Option<Self> {
        (minimum <= maximum).then_some(Self { minimum, maximum })
    }

    pub const fn minimum(self) -> TipAmount {
        self.minimum
    }

    pub const fn maximum(self) -> TipAmount {
        self.maximum
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultPostTipPolicy {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PublicationTipSettings {
    Unconfigured,
    Configured {
        default: DefaultPostTipPolicy,
        range: TipAmountRange,
    },
}

impl PublicationTipSettings {
    pub const fn is_configured(self) -> bool {
        matches!(self, Self::Configured { .. })
    }

    pub const fn default_policy(self) -> DefaultPostTipPolicy {
        match self {
            Self::Unconfigured => DefaultPostTipPolicy::Disabled,
            Self::Configured { default, .. } => default,
        }
    }

    pub const fn range(self) -> Option<TipAmountRange> {
        match self {
            Self::Unconfigured => None,
            Self::Configured { range, .. } => Some(range),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubscriptionSettings {
    Disabled,
    Enabled {
        privacy_policy_revision: PrivacyPolicyRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SiteSettings {
    title: SiteTitle,
    base_url: PublicationBaseUrl,
    description: SiteDescription,
    favicon: Option<UnresolvedAssetReference>,
}

impl SiteSettings {
    pub(crate) const fn new(
        title: SiteTitle,
        base_url: PublicationBaseUrl,
        description: SiteDescription,
        favicon: Option<UnresolvedAssetReference>,
    ) -> Self {
        Self {
            title,
            base_url,
            description,
            favicon,
        }
    }

    pub const fn title(&self) -> &SiteTitle {
        &self.title
    }

    pub const fn base_url(&self) -> &PublicationBaseUrl {
        &self.base_url
    }

    pub const fn description(&self) -> &SiteDescription {
        &self.description
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by asset resolution in WP 1.6")
    )]
    pub(crate) const fn favicon(&self) -> Option<&UnresolvedAssetReference> {
        self.favicon.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuthorSettings {
    name: AuthorName,
}

impl AuthorSettings {
    pub(crate) const fn new(name: AuthorName) -> Self {
        Self { name }
    }

    pub const fn name(&self) -> &AuthorName {
        &self.name
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PublicationAssetSettings {
    allowed_https_origins: Vec<UnresolvedHttpsOrigin>,
}

impl PublicationAssetSettings {
    pub(crate) fn new(allowed_https_origins: Vec<UnresolvedHttpsOrigin>) -> Self {
        Self {
            allowed_https_origins,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by asset resolution in WP 1.6")
    )]
    pub(crate) fn allowed_https_origins(&self) -> &[UnresolvedHttpsOrigin] {
        &self.allowed_https_origins
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicationSettings {
    site: SiteSettings,
    author: AuthorSettings,
    assets: PublicationAssetSettings,
    subscriptions: SubscriptionSettings,
    tips: PublicationTipSettings,
    renderer: RendererSettings,
}

impl PublicationSettings {
    pub(crate) const fn new(
        site: SiteSettings,
        author: AuthorSettings,
        assets: PublicationAssetSettings,
        subscriptions: SubscriptionSettings,
        tips: PublicationTipSettings,
        renderer: RendererSettings,
    ) -> Self {
        Self {
            site,
            author,
            assets,
            subscriptions,
            tips,
            renderer,
        }
    }

    pub const fn site(&self) -> &SiteSettings {
        &self.site
    }

    pub const fn author(&self) -> &AuthorSettings {
        &self.author
    }

    pub const fn assets(&self) -> &PublicationAssetSettings {
        &self.assets
    }

    pub const fn subscriptions(&self) -> &SubscriptionSettings {
        &self.subscriptions
    }

    pub const fn tips(&self) -> PublicationTipSettings {
        self.tips
    }

    pub const fn renderer(&self) -> RendererSettings {
        self.renderer
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PostMetadata {
    id: PostId,
    title: PostTitle,
    slug: PostSlug,
    #[serde(with = "time::serde::rfc3339")]
    authored_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    updated_at: Option<OffsetDateTime>,
    description: PostDescription,
    image: Option<UnresolvedAssetReference>,
    tags: Vec<PostTag>,
    aliases: Vec<PostAlias>,
    draft: DraftStatus,
    tips: PostTipPolicy,
    distribution: DistributionSettings,
}

impl PostMetadata {
    pub(crate) fn new(parts: PostMetadataParts) -> Self {
        Self {
            id: parts.id,
            title: parts.title,
            slug: parts.slug,
            authored_at: parts.authored_at,
            updated_at: parts.updated_at,
            description: parts.description,
            image: parts.image,
            tags: parts.tags,
            aliases: parts.aliases,
            draft: parts.draft,
            tips: parts.tips,
            distribution: parts.distribution,
        }
    }

    pub const fn id(&self) -> &PostId {
        &self.id
    }

    pub const fn title(&self) -> &PostTitle {
        &self.title
    }

    pub const fn slug(&self) -> &PostSlug {
        &self.slug
    }

    pub const fn authored_at(&self) -> OffsetDateTime {
        self.authored_at
    }

    pub const fn updated_at(&self) -> Option<OffsetDateTime> {
        self.updated_at
    }

    pub const fn description(&self) -> &PostDescription {
        &self.description
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by asset resolution in WP 1.6")
    )]
    pub(crate) const fn image(&self) -> Option<&UnresolvedAssetReference> {
        self.image.as_ref()
    }

    pub fn tags(&self) -> &[PostTag] {
        &self.tags
    }

    pub fn aliases(&self) -> &[PostAlias] {
        &self.aliases
    }

    pub const fn draft(&self) -> DraftStatus {
        self.draft
    }

    pub const fn tips(&self) -> PostTipPolicy {
        self.tips
    }

    pub const fn distribution(&self) -> &DistributionSettings {
        &self.distribution
    }
}

pub(crate) struct PostMetadataParts {
    pub(crate) id: PostId,
    pub(crate) title: PostTitle,
    pub(crate) slug: PostSlug,
    pub(crate) authored_at: OffsetDateTime,
    pub(crate) updated_at: Option<OffsetDateTime>,
    pub(crate) description: PostDescription,
    pub(crate) image: Option<UnresolvedAssetReference>,
    pub(crate) tags: Vec<PostTag>,
    pub(crate) aliases: Vec<PostAlias>,
    pub(crate) draft: DraftStatus,
    pub(crate) tips: PostTipPolicy,
    pub(crate) distribution: DistributionSettings,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PostDocument {
    path: LogicalContentPath,
    metadata: PostMetadata,
    markdown: MarkdownSource,
}

impl PostDocument {
    pub(crate) const fn new(
        path: LogicalContentPath,
        metadata: PostMetadata,
        markdown: MarkdownSource,
    ) -> Self {
        Self {
            path,
            metadata,
            markdown,
        }
    }

    pub const fn path(&self) -> &LogicalContentPath {
        &self.path
    }

    pub const fn metadata(&self) -> &PostMetadata {
        &self.metadata
    }

    pub const fn markdown(&self) -> &MarkdownSource {
        &self.markdown
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidatedContent {
    publication: PublicationSettings,
    posts: Vec<PostDocument>,
}

impl ValidatedContent {
    pub(crate) const fn new(publication: PublicationSettings, posts: Vec<PostDocument>) -> Self {
        Self { publication, posts }
    }

    pub const fn publication(&self) -> &PublicationSettings {
        &self.publication
    }

    pub fn posts(&self) -> &[PostDocument] {
        &self.posts
    }
}
