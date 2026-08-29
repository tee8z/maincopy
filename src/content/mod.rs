//! Git-owned content types and compilation.

mod model;
mod parser;
mod validation;

pub use model::{
    AuthorName, AuthorSettings, CodeRenderingMode, DefaultPostTipPolicy, DistributionCopy,
    DistributionMode, DistributionSettings, DraftStatus, LogicalContentPath, MarkdownDialect,
    MarkdownSource, MermaidRenderingMode, PlainTextError, PostAlias, PostDescription, PostDocument,
    PostId, PostIdParseError, PostMetadata, PostSlug, PostSource, PostTag, PostTipPolicy,
    PostTitle, PrivacyPolicyRevision, PublicationAssetSettings, PublicationBaseUrl,
    PublicationBaseUrlError, PublicationSettings, PublicationSource, PublicationTipSettings,
    RawHtmlPolicy, RendererSettings, RouteValueError, SiteDescription, SiteSettings, SiteTitle,
    SubscriptionSettings, TipAmount, TipAmountRange, ValidatedContent, XDistributionSettings,
};
pub use parser::validate_content;
pub use validation::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, FieldPath,
    ValidationLocation,
};

pub(crate) use model::{PostMetadataParts, UnresolvedAssetReference, UnresolvedHttpsOrigin};
pub(crate) use validation::DiagnosticCollector;

#[cfg(test)]
mod tests;
