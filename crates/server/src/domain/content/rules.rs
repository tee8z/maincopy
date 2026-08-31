use time::OffsetDateTime;

use crate::content::PostCollection;

use super::{
    DefaultPostTipPolicy, DraftStatus, PostTipPolicy, PrivacyPolicyRevision,
    PublicationTipSettings, SubscriptionSettings, TipAmount, TipAmountRange,
};

pub(crate) fn timestamps_are_ordered(
    authored_at: OffsetDateTime,
    updated_at: OffsetDateTime,
) -> bool {
    updated_at >= authored_at
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DraftStatusResolution {
    pub(crate) status: DraftStatus,
    pub(crate) conflicts_with_collection: bool,
}

pub(crate) const fn resolve_draft_status(
    collection: PostCollection,
    authored: Option<bool>,
) -> DraftStatusResolution {
    match (collection, authored) {
        (PostCollection::Drafts, Some(false)) => DraftStatusResolution {
            status: DraftStatus::Draft,
            conflicts_with_collection: true,
        },
        (PostCollection::Drafts, _) => DraftStatusResolution {
            status: DraftStatus::Draft,
            conflicts_with_collection: false,
        },
        (PostCollection::Posts, Some(true)) => DraftStatusResolution {
            status: DraftStatus::Draft,
            conflicts_with_collection: false,
        },
        (PostCollection::Posts, None | Some(false)) => DraftStatusResolution {
            status: DraftStatus::Publishable,
            conflicts_with_collection: false,
        },
    }
}

pub(crate) const fn post_tip_policy(authored: Option<bool>) -> PostTipPolicy {
    match authored {
        None => PostTipPolicy::InheritPublication,
        Some(true) => PostTipPolicy::Enabled,
        Some(false) => PostTipPolicy::Disabled,
    }
}

pub(crate) fn subscription_settings(
    enabled: bool,
    privacy_policy_revision: Option<PrivacyPolicyRevision>,
) -> Result<SubscriptionSettings, ()> {
    if !enabled {
        return Ok(SubscriptionSettings::Disabled);
    }
    privacy_policy_revision
        .map(|privacy_policy_revision| SubscriptionSettings::Enabled {
            privacy_policy_revision,
        })
        .ok_or(())
}

pub(crate) fn configured_publication_tips(
    enabled: bool,
    minimum: TipAmount,
    maximum: TipAmount,
) -> Option<PublicationTipSettings> {
    TipAmountRange::new(minimum, maximum).map(|range| PublicationTipSettings::Configured {
        default: if enabled {
            DefaultPostTipPolicy::Enabled
        } else {
            DefaultPostTipPolicy::Disabled
        },
        range,
    })
}

pub(crate) const fn post_tips_are_supported(
    publication: PublicationTipSettings,
    post: PostTipPolicy,
) -> bool {
    publication.is_configured() || !matches!(post, PostTipPolicy::Enabled)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum RouteKind {
    Canonical,
    Alias,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteConflict {
    DuplicateSlug,
    DuplicateAlias,
    AliasMatchesSlug,
    DuplicateRoute,
}

pub(crate) const fn classify_route_conflict(
    anchor: RouteKind,
    duplicate: RouteKind,
    same_post: bool,
) -> RouteConflict {
    match (anchor, duplicate) {
        (RouteKind::Canonical, RouteKind::Canonical) => RouteConflict::DuplicateSlug,
        (RouteKind::Alias, RouteKind::Alias) => RouteConflict::DuplicateAlias,
        _ if same_post => RouteConflict::AliasMatchesSlug,
        _ => RouteConflict::DuplicateRoute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_collection_cannot_be_overridden_to_publishable() {
        assert_eq!(
            resolve_draft_status(PostCollection::Drafts, Some(false)),
            DraftStatusResolution {
                status: DraftStatus::Draft,
                conflicts_with_collection: true,
            }
        );
    }

    #[test]
    fn route_conflicts_are_classified_by_semantics() {
        assert_eq!(
            classify_route_conflict(RouteKind::Canonical, RouteKind::Alias, true),
            RouteConflict::AliasMatchesSlug
        );
        assert_eq!(
            classify_route_conflict(RouteKind::Canonical, RouteKind::Alias, false),
            RouteConflict::DuplicateRoute
        );
    }
}
