//! Authored-content domain values and semantic rules.
//!
//! Source discovery, parsing, diagnostics, and asset resolution remain in
//! [`crate::content`]. This module owns the valid authored model that those
//! mechanisms construct.

mod model;
mod rules;

pub use model::{
    AuthorName, AuthorSettings, DefaultPostTipPolicy, DistributionCopy, DistributionMode,
    DistributionSettings, DraftStatus, MarkdownSource, PlainTextError, PostAlias, PostDescription,
    PostDocument, PostId, PostIdParseError, PostMetadata, PostSlug, PostTag, PostTipPolicy,
    PostTitle, PrivacyPolicyRevision, PublicationBaseUrl, PublicationBaseUrlError,
    PublicationSettings, PublicationTipSettings, RouteValueError, SiteDescription, SiteSettings,
    SiteTitle, SubscriptionSettings, TipAmount, TipAmountRange, ValidatedContent,
    XDistributionSettings,
};

pub(crate) use model::{
    PublicationAssetSettings, UnresolvedAssetReference, UnresolvedHttpsOrigin,
};
pub(crate) use rules::{
    DraftStatusResolution, RouteConflict, RouteKind, classify_route_conflict,
    configured_publication_tips, post_tip_policy, post_tips_are_supported, resolve_draft_status,
    subscription_settings, timestamps_are_ordered,
};
