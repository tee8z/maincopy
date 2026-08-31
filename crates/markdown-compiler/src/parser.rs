use std::collections::BTreeMap;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use toml::{Table, Value};

use crate::content::{
    AuthorName, AuthorSettings, DefaultPostTipPolicy, DistributionCopy, DistributionMode,
    DistributionSettings, DraftStatus, MarkdownSource, PlainTextError, PostAlias, PostDescription,
    PostDocument, PostId, PostMetadata, PostSlug, PostTag, PostTipPolicy, PostTitle,
    PrivacyPolicyRevision, PublicationAssetSettings, PublicationBaseUrl, PublicationSettings,
    RouteConflict, RouteKind, SiteDescription, SiteSettings, SiteTitle, UnresolvedAssetReference,
    UnresolvedHttpsOrigin, ValidatedContent, XDistributionSettings, classify_route_conflict,
    resolve_draft_status as resolve_authored_draft_status, subscription_settings,
    timestamps_are_ordered,
};

use super::{
    ContentValidationCode, ContentValidationError, ContentValidationErrors, DiagnosticCollector,
    LogicalContentPath, PostCollection, PostSource, PublicationSource, ValidationLocation,
};

pub fn validate_content<'source>(
    publication: PublicationSource<'source>,
    posts: impl IntoIterator<Item = PostSource<'source>>,
) -> Result<ValidatedContent, ContentValidationErrors> {
    let mut diagnostics = DiagnosticCollector::default();
    let publication = parse_publication(publication, &mut diagnostics);

    let mut post_sources: Vec<_> = posts.into_iter().collect();
    post_sources.sort_by(|left, right| left.path.cmp(&right.path));
    let post_candidates: Vec<_> = post_sources
        .into_iter()
        .enumerate()
        .map(|(source_index, source)| parse_post(source_index, source, &mut diagnostics))
        .collect();

    validate_post_identities(&post_candidates, &mut diagnostics);
    validate_post_routes(&post_candidates, &mut diagnostics);

    let had_no_reported_errors = diagnostics.is_empty();
    if had_no_reported_errors && publication.settings.is_none() {
        diagnostics.push(invariant_error(
            publication_path(&publication),
            "validated publication settings were not constructed",
        ));
    }
    if had_no_reported_errors {
        for post in &post_candidates {
            if post.document.is_none() {
                diagnostics.push(invariant_error(
                    post.path.clone(),
                    "validated post document was not constructed",
                ));
            }
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics.finish());
    }

    let publication_path = publication.path.clone();
    let publication = match publication.settings {
        Some(publication) => publication,
        None => return Err(single_invariant_error(publication_path)),
    };
    let mut posts = Vec::with_capacity(post_candidates.len());
    for candidate in post_candidates {
        match candidate.document {
            Some(document) => posts.push(document),
            None => return Err(single_invariant_error(candidate.path)),
        }
    }
    Ok(ValidatedContent::new(publication, posts))
}

/// Validate one Markdown post without requiring its diagnostic label to be a
/// managed-tree path.
///
/// The supplied collection still controls draft semantics. Intrinsic metadata
/// checks and conflicts among the post's own slug and aliases are enforced,
/// while logical placement, collection-directory, and filename checks are
/// deliberately skipped.
pub fn validate_post_document(
    path_label: impl Into<String>,
    contents: &str,
    collection: PostCollection,
) -> Result<PostDocument, ContentValidationErrors> {
    let mut diagnostics = DiagnosticCollector::default();
    let candidate = parse_post_with_placement(
        0,
        PostSource {
            path: LogicalContentPath::new(path_label),
            contents,
            collection,
        },
        &mut diagnostics,
        false,
    );

    validate_post_identities(std::slice::from_ref(&candidate), &mut diagnostics);
    validate_post_routes(std::slice::from_ref(&candidate), &mut diagnostics);

    if diagnostics.is_empty() && candidate.document.is_none() {
        diagnostics.push(invariant_error(
            candidate.path.clone(),
            "validated post document was not constructed",
        ));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics.finish());
    }
    candidate
        .document
        .ok_or_else(|| single_invariant_error(candidate.path))
}

/// Validate one Markdown post from raw bytes using the production default post
/// byte limit and UTF-8 contract.
pub fn validate_post_document_bytes(
    path_label: impl Into<String>,
    bytes: &[u8],
    collection: PostCollection,
) -> Result<PostDocument, ContentValidationErrors> {
    let path_label = path_label.into();
    if bytes.len() > crate::tree::DEFAULT_POST_BYTES as usize {
        return Err(single_document_error(
            &path_label,
            ContentValidationCode::ContentFileTooLarge,
            "managed content file exceeds its configured byte limit",
        ));
    }
    let contents = std::str::from_utf8(bytes).map_err(|_| {
        single_document_error(
            &path_label,
            ContentValidationCode::ContentTextInvalidUtf8,
            "publication and post source files must contain UTF-8 text",
        )
    })?;
    validate_post_document(path_label, contents, collection)
}

fn single_document_error(
    path: &str,
    code: ContentValidationCode,
    message: &'static str,
) -> ContentValidationErrors {
    let mut diagnostics = DiagnosticCollector::default();
    diagnostics.push(ContentValidationError::new(
        LogicalContentPath::new(path),
        "$document",
        code,
        message,
    ));
    diagnostics.finish()
}

struct PublicationCandidate {
    path: LogicalContentPath,
    settings: Option<PublicationSettings>,
}

fn publication_path(publication: &PublicationCandidate) -> LogicalContentPath {
    publication.path.clone()
}

fn parse_publication(
    source: PublicationSource<'_>,
    diagnostics: &mut DiagnosticCollector,
) -> PublicationCandidate {
    let start_error_count = diagnostics.len();
    let path = source.path.clone();
    let mut table = match source.contents.parse::<Table>() {
        Ok(table) => table,
        Err(error) => {
            diagnostics.push(ContentValidationError::new(
                path,
                "$document",
                ContentValidationCode::PublicationTomlInvalid,
                format!("publication TOML is invalid: {error}"),
            ));
            return PublicationCandidate {
                path: source.path,
                settings: None,
            };
        }
    };

    let site =
        take_required_table(&mut table, "site", "site", &path, diagnostics).and_then(|mut site| {
            let title = take_required_string(&mut site, "title", "site.title", &path, diagnostics)
                .and_then(|value| {
                    parse_plain_text(value, SiteTitle::new, "site.title", &path, diagnostics)
                });
            let base_url =
                take_required_string(&mut site, "base_url", "site.base_url", &path, diagnostics)
                    .and_then(|value| match PublicationBaseUrl::parse(&value) {
                        Ok(value) => Some(value),
                        Err(error) => {
                            diagnostics.push(ContentValidationError::new(
                                path.clone(),
                                "site.base_url",
                                ContentValidationCode::InvalidBaseUrl,
                                error.to_string(),
                            ));
                            None
                        }
                    });
            let description = take_required_string(
                &mut site,
                "description",
                "site.description",
                &path,
                diagnostics,
            )
            .and_then(|value| {
                parse_plain_text(
                    value,
                    SiteDescription::new,
                    "site.description",
                    &path,
                    diagnostics,
                )
            });
            let favicon =
                take_optional_string(&mut site, "favicon", "site.favicon", &path, diagnostics)
                    .and_then(|value| {
                        parse_plain_text(
                            value,
                            UnresolvedAssetReference::new,
                            "site.favicon",
                            &path,
                            diagnostics,
                        )
                    });
            reject_unknown_fields(site, "site", &path, diagnostics);
            Some(SiteSettings::new(title?, base_url?, description?, favicon))
        });

    let author = take_required_table(&mut table, "author", "author", &path, diagnostics).and_then(
        |mut author| {
            let name = take_required_string(&mut author, "name", "author.name", &path, diagnostics)
                .and_then(|value| {
                    parse_plain_text(value, AuthorName::new, "author.name", &path, diagnostics)
                });
            reject_unknown_fields(author, "author", &path, diagnostics);
            name.map(AuthorSettings::new)
        },
    );

    let assets = match take_optional_table(&mut table, "assets", "assets", &path, diagnostics) {
        OptionalField::Missing => Some(PublicationAssetSettings::default()),
        OptionalField::Invalid => None,
        OptionalField::Valid(mut assets) => {
            let origins = take_optional_string_array(
                &mut assets,
                "allowed_https_origins",
                "assets.allowed_https_origins",
                &path,
                diagnostics,
            )
            .into_iter()
            .filter_map(|(index, value)| {
                parse_plain_text(
                    value,
                    UnresolvedHttpsOrigin::new,
                    &indexed_field("assets.allowed_https_origins", index),
                    &path,
                    diagnostics,
                )
            })
            .collect();
            reject_unknown_fields(assets, "assets", &path, diagnostics);
            Some(PublicationAssetSettings {
                allowed_https_origins: origins,
            })
        }
    };

    let subscriptions = parse_subscriptions(&mut table, &path, diagnostics);
    let tips = parse_publication_tips(&mut table, &path, diagnostics);
    reject_unknown_fields(table, "", &path, diagnostics);

    let settings = if diagnostics.len() == start_error_count {
        match (site, author, assets, subscriptions, tips) {
            (Some(site), Some(author), Some(assets), Some(subscriptions), Some(tips)) => Some(
                PublicationSettings::new(site, author, assets, subscriptions, tips),
            ),
            _ => {
                diagnostics.push(invariant_error(
                    path.clone(),
                    "publication validation succeeded without all required typed fields",
                ));
                None
            }
        }
    } else {
        None
    };

    PublicationCandidate { path, settings }
}

fn parse_subscriptions(
    table: &mut Table,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<super::SubscriptionSettings> {
    let mut subscriptions =
        match take_optional_table(table, "subscriptions", "subscriptions", path, diagnostics) {
            OptionalField::Missing => return Some(super::SubscriptionSettings::Disabled),
            OptionalField::Invalid => return None,
            OptionalField::Valid(value) => value,
        };

    let enabled = take_optional_bool(
        &mut subscriptions,
        "enabled",
        "subscriptions.enabled",
        path,
        diagnostics,
    );
    let revision = take_optional_string(
        &mut subscriptions,
        "privacy_policy_revision",
        "subscriptions.privacy_policy_revision",
        path,
        diagnostics,
    )
    .and_then(|value| {
        parse_plain_text(
            value,
            PrivacyPolicyRevision::new,
            "subscriptions.privacy_policy_revision",
            path,
            diagnostics,
        )
    });
    reject_unknown_fields(subscriptions, "subscriptions", path, diagnostics);

    let enabled = match enabled {
        OptionalField::Missing => false,
        OptionalField::Valid(enabled) => enabled,
        OptionalField::Invalid => return None,
    };
    match subscription_settings(enabled, revision) {
        Ok(settings) => Some(settings),
        Err(()) => {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                "subscriptions.privacy_policy_revision",
                ContentValidationCode::SubscriptionPrivacyRevisionRequired,
                "an enabled subscription policy requires a privacy-policy revision",
            ));
            None
        }
    }
}

fn parse_publication_tips(
    table: &mut Table,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<DefaultPostTipPolicy> {
    let mut tips = match take_optional_table(table, "tips", "tips", path, diagnostics) {
        OptionalField::Missing => return Some(DefaultPostTipPolicy::Disabled),
        OptionalField::Invalid => return None,
        OptionalField::Valid(value) => value,
    };
    let enabled = take_optional_bool(&mut tips, "enabled", "tips.enabled", path, diagnostics);
    reject_unknown_fields(tips, "tips", path, diagnostics);

    let enabled = match enabled {
        OptionalField::Missing => false,
        OptionalField::Valid(value) => value,
        OptionalField::Invalid => return None,
    };
    Some(match enabled {
        true => DefaultPostTipPolicy::Enabled,
        false => DefaultPostTipPolicy::Disabled,
    })
}

struct PostCandidate {
    source_index: usize,
    path: LogicalContentPath,
    document: Option<PostDocument>,
    id: Option<PostId>,
    slug: Option<PostSlug>,
    aliases: Vec<(usize, PostAlias)>,
}

fn parse_post(
    source_index: usize,
    source: PostSource<'_>,
    diagnostics: &mut DiagnosticCollector,
) -> PostCandidate {
    parse_post_with_placement(source_index, source, diagnostics, true)
}

fn parse_post_with_placement(
    source_index: usize,
    source: PostSource<'_>,
    diagnostics: &mut DiagnosticCollector,
    validate_placement: bool,
) -> PostCandidate {
    let start_error_count = diagnostics.len();
    let path = source.path.clone();

    if validate_placement {
        validate_post_source_path(&source, &path, diagnostics);
    }
    let Some((mut table, markdown)) = parse_post_frontmatter(source.contents, &path, diagnostics)
    else {
        return PostCandidate {
            source_index,
            path,
            document: None,
            id: None,
            slug: None,
            aliases: Vec::new(),
        };
    };

    if table.remove("published_at").is_some() {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "published_at",
            ContentValidationCode::PublishedAtUnsupported,
            "published_at is SQLite-owned policy and is not allowed in frontmatter",
        ));
    }

    let id = take_required_string(&mut table, "id", "id", &path, diagnostics).and_then(|value| {
        match PostId::parse(&value) {
            Ok(value) => Some(value),
            Err(error) => {
                diagnostics.push(ContentValidationError::new(
                    path.clone(),
                    "id",
                    ContentValidationCode::InvalidPostId,
                    error.to_string(),
                ));
                None
            }
        }
    });
    let title = take_required_string(&mut table, "title", "title", &path, diagnostics)
        .and_then(|value| parse_plain_text(value, PostTitle::new, "title", &path, diagnostics));
    let slug =
        take_required_string(&mut table, "slug", "slug", &path, diagnostics).and_then(|value| {
            match PostSlug::parse(value) {
                Ok(value) => Some(value),
                Err(error) => {
                    diagnostics.push(ContentValidationError::new(
                        path.clone(),
                        "slug",
                        ContentValidationCode::InvalidPostSlug,
                        error.to_string(),
                    ));
                    None
                }
            }
        });
    let authored_at =
        take_required_datetime(&mut table, "authored_at", "authored_at", &path, diagnostics);
    let updated_at =
        take_optional_datetime(&mut table, "updated_at", "updated_at", &path, diagnostics);
    if let (Some(authored_at), OptionalField::Valid(updated_at)) = (authored_at, updated_at)
        && !timestamps_are_ordered(authored_at, updated_at)
    {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "updated_at",
            ContentValidationCode::UpdatedAtBeforeAuthoredAt,
            "updated_at must not be earlier than authored_at",
        ));
    }
    let updated_at_value = updated_at.into_option();
    let description =
        take_required_string(&mut table, "description", "description", &path, diagnostics)
            .and_then(|value| {
                parse_plain_text(
                    value,
                    PostDescription::new,
                    "description",
                    &path,
                    diagnostics,
                )
            });
    let image =
        take_optional_string(&mut table, "image", "image", &path, diagnostics).and_then(|value| {
            parse_plain_text(
                value,
                UnresolvedAssetReference::new,
                "image",
                &path,
                diagnostics,
            )
        });
    let tags = parse_post_tags(&mut table, &path, diagnostics);
    let aliases: Vec<_> =
        take_optional_string_array(&mut table, "aliases", "aliases", &path, diagnostics)
            .into_iter()
            .filter_map(|(index, value)| match PostAlias::parse(value) {
                Ok(alias) => Some((index, alias)),
                Err(error) => {
                    diagnostics.push(ContentValidationError::new(
                        path.clone(),
                        indexed_field("aliases", index),
                        ContentValidationCode::InvalidPostAlias,
                        error.to_string(),
                    ));
                    None
                }
            })
            .collect();
    let authored_draft = take_optional_bool(&mut table, "draft", "draft", &path, diagnostics);
    let draft = resolve_draft_status(source.collection, authored_draft, &path, diagnostics);
    let tips = match take_optional_bool(&mut table, "tips", "tips", &path, diagnostics) {
        OptionalField::Missing => Some(PostTipPolicy::InheritPublication),
        OptionalField::Valid(true) => Some(PostTipPolicy::Enabled),
        OptionalField::Valid(false) => Some(PostTipPolicy::Disabled),
        OptionalField::Invalid => None,
    };
    let distribution = parse_distribution(&mut table, &path, diagnostics);
    reject_unknown_fields(table, "", &path, diagnostics);

    let document = if diagnostics.len() == start_error_count {
        match (
            id.clone(),
            title,
            slug.clone(),
            authored_at,
            description,
            tips,
            distribution,
        ) {
            (
                Some(id),
                Some(title),
                Some(slug),
                Some(authored_at),
                Some(description),
                Some(tips),
                Some(distribution),
            ) => Some(PostDocument::new(
                path.clone(),
                PostMetadata {
                    id,
                    title,
                    slug,
                    authored_at,
                    updated_at: updated_at_value,
                    description,
                    image,
                    tags,
                    aliases: aliases.iter().map(|(_, alias)| alias.clone()).collect(),
                    draft,
                    tips,
                    distribution,
                },
                MarkdownSource::new(markdown),
            )),
            _ => {
                diagnostics.push(invariant_error(
                    path.clone(),
                    "post validation succeeded without all required typed fields",
                ));
                None
            }
        }
    } else {
        None
    };

    PostCandidate {
        source_index,
        path,
        document,
        id,
        slug,
        aliases,
    }
}

fn validate_post_source_path(
    source: &PostSource<'_>,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) {
    if super::path::PortableLogicalPath::parse(path.as_str(), usize::MAX).is_err() {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "$path",
            ContentValidationCode::InvalidLogicalContentPath,
            "post source path must use portable logical-path components",
        ));
    }
    if !source.collection.contains_path(path.as_str()) {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "$path",
            ContentValidationCode::PostCollectionPathMismatch,
            format!(
                "post source collection requires a path below {}/",
                source.collection.directory()
            ),
        ));
    }
    if !path
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|name| name.ends_with(".md"))
    {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "$path",
            ContentValidationCode::UnexpectedPostEntry,
            "post source path must use the exact lowercase .md suffix",
        ));
    }
}

fn parse_post_frontmatter<'source>(
    contents: &'source str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<(Table, &'source str)> {
    let (frontmatter, markdown) = split_frontmatter(contents, path, diagnostics)?;
    let table = match frontmatter.parse::<Table>() {
        Ok(table) => table,
        Err(error) => {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                "$frontmatter",
                ContentValidationCode::FrontmatterTomlInvalid,
                format!("frontmatter TOML is invalid: {error}"),
            ));
            return None;
        }
    };
    Some((table, markdown))
}

fn parse_post_tags(
    table: &mut Table,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Vec<PostTag> {
    let mut first_tag_indexes = BTreeMap::new();
    take_optional_string_array(table, "tags", "tags", path, diagnostics)
        .into_iter()
        .filter_map(|(index, value)| match PostTag::parse(value) {
            Ok(tag) => {
                if let Some(first_index) = first_tag_indexes.get(&tag).copied() {
                    diagnostics.push(
                        ContentValidationError::new(
                            path.clone(),
                            indexed_field("tags", index),
                            ContentValidationCode::DuplicateTag,
                            "tag duplicates an earlier normalized tag",
                        )
                        .with_related(ValidationLocation::new(
                            path.clone(),
                            super::FieldPath::new(indexed_field("tags", first_index)),
                        )),
                    );
                } else {
                    first_tag_indexes.insert(tag.clone(), index);
                }
                Some(tag)
            }
            Err(error) => {
                diagnostics.push(ContentValidationError::new(
                    path.clone(),
                    indexed_field("tags", index),
                    ContentValidationCode::InvalidPostTag,
                    error.to_string(),
                ));
                None
            }
        })
        .collect()
}

fn resolve_draft_status(
    collection: PostCollection,
    authored: OptionalField<bool>,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> DraftStatus {
    let authored = match authored {
        OptionalField::Valid(authored) => Some(authored),
        OptionalField::Missing | OptionalField::Invalid => None,
    };
    let resolution = resolve_authored_draft_status(collection, authored);
    if resolution.conflicts_with_collection {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "draft",
            ContentValidationCode::DraftDirectoryConflict,
            "a post in drafts/ cannot set draft to false",
        ));
    }
    resolution.status
}

fn parse_distribution(
    table: &mut Table,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<DistributionSettings> {
    let mut distribution =
        match take_optional_table(table, "distribution", "distribution", path, diagnostics) {
            OptionalField::Missing => return Some(DistributionSettings::default()),
            OptionalField::Invalid => return None,
            OptionalField::Valid(value) => value,
        };
    let x = match take_optional_table(&mut distribution, "x", "distribution.x", path, diagnostics) {
        OptionalField::Missing => Some(XDistributionSettings::default()),
        OptionalField::Invalid => None,
        OptionalField::Valid(mut x) => {
            let enabled = take_required_bool(
                &mut x,
                "enabled",
                "distribution.x.enabled",
                path,
                diagnostics,
                ContentValidationCode::DistributionEnabledRequired,
            );
            let copy =
                take_optional_string(&mut x, "text", "distribution.x.text", path, diagnostics)
                    .and_then(|value| {
                        parse_plain_text(
                            value,
                            DistributionCopy::new,
                            "distribution.x.text",
                            path,
                            diagnostics,
                        )
                    });
            reject_unknown_fields(x, "distribution.x", path, diagnostics);
            Some(XDistributionSettings::new(
                if enabled? {
                    DistributionMode::Enabled
                } else {
                    DistributionMode::Disabled
                },
                copy,
            ))
        }
    };
    reject_unknown_fields(distribution, "distribution", path, diagnostics);
    x.map(DistributionSettings::new)
}

fn split_frontmatter<'source>(
    contents: &'source str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<(&'source str, &'source str)> {
    let (opening, frontmatter_start) = next_line(contents, 0);
    if opening != "+++" {
        let (code, message) = if looks_like_delimiter(opening) {
            (
                ContentValidationCode::FrontmatterOpeningDelimiterMalformed,
                "opening frontmatter delimiter must be exactly +++",
            )
        } else {
            (
                ContentValidationCode::FrontmatterOpeningDelimiterMissing,
                "post must begin with a +++ frontmatter delimiter",
            )
        };
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            "$frontmatter",
            code,
            message,
        ));
        return None;
    }

    let mut cursor = frontmatter_start;
    while cursor < contents.len() {
        let line_start = cursor;
        let (line, next) = next_line(contents, cursor);
        if line == "+++" {
            return Some((&contents[frontmatter_start..line_start], &contents[next..]));
        }
        if looks_like_delimiter(line) {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                "$frontmatter",
                ContentValidationCode::FrontmatterClosingDelimiterMalformed,
                "closing frontmatter delimiter must be exactly +++",
            ));
            return None;
        }
        cursor = next;
    }

    diagnostics.push(ContentValidationError::new(
        path.clone(),
        "$frontmatter",
        ContentValidationCode::FrontmatterClosingDelimiterMissing,
        "frontmatter has no closing +++ delimiter",
    ));
    None
}

fn next_line(contents: &str, start: usize) -> (&str, usize) {
    let remaining = &contents[start..];
    let next = remaining
        .find('\n')
        .map_or(contents.len(), |relative| start + relative + 1);
    let mut line_end = next;
    if line_end > start && contents.as_bytes()[line_end - 1] == b'\n' {
        line_end -= 1;
    }
    if line_end > start && contents.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    (&contents[start..line_end], next)
}

fn looks_like_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("+++") || (trimmed.len() >= 3 && trimmed.bytes().all(|byte| byte == b'+'))
}

fn validate_post_identities(posts: &[PostCandidate], diagnostics: &mut DiagnosticCollector) {
    let mut identities: BTreeMap<PostId, Vec<&PostCandidate>> = BTreeMap::new();
    for post in posts {
        if let Some(id) = &post.id {
            identities.entry(id.clone()).or_default().push(post);
        }
    }
    for duplicates in identities.into_values().filter(|values| values.len() > 1) {
        let anchor = duplicates[0];
        for duplicate in duplicates.into_iter().skip(1) {
            diagnostics.push(
                ContentValidationError::new(
                    duplicate.path.clone(),
                    "id",
                    ContentValidationCode::DuplicatePostId,
                    "post ID duplicates an earlier post",
                )
                .with_related(ValidationLocation::new(
                    anchor.path.clone(),
                    super::FieldPath::new("id"),
                )),
            );
        }
    }
}

struct RouteLocation<'candidate> {
    post: &'candidate PostCandidate,
    field: String,
    kind: RouteKind,
}

fn validate_post_routes(posts: &[PostCandidate], diagnostics: &mut DiagnosticCollector) {
    let mut routes: BTreeMap<&str, Vec<RouteLocation<'_>>> = BTreeMap::new();
    for post in posts {
        if let Some(slug) = &post.slug {
            routes
                .entry(slug.as_str())
                .or_default()
                .push(RouteLocation {
                    post,
                    field: "slug".to_owned(),
                    kind: RouteKind::Canonical,
                });
        }
        for (index, alias) in &post.aliases {
            routes
                .entry(alias.as_str())
                .or_default()
                .push(RouteLocation {
                    post,
                    field: indexed_field("aliases", *index),
                    kind: RouteKind::Alias,
                });
        }
    }

    for mut duplicates in routes.into_values().filter(|values| values.len() > 1) {
        duplicates.sort_by(|left, right| {
            (
                left.post.path.as_str(),
                left.post.source_index,
                left.kind,
                left.field.as_str(),
            )
                .cmp(&(
                    right.post.path.as_str(),
                    right.post.source_index,
                    right.kind,
                    right.field.as_str(),
                ))
        });
        let anchor = &duplicates[0];
        for duplicate in duplicates.iter().skip(1) {
            let (code, message) = match classify_route_conflict(
                anchor.kind,
                duplicate.kind,
                anchor.post.source_index == duplicate.post.source_index,
            ) {
                RouteConflict::DuplicateSlug => (
                    ContentValidationCode::DuplicatePostSlug,
                    "canonical slug duplicates an earlier post slug",
                ),
                RouteConflict::DuplicateAlias => (
                    ContentValidationCode::DuplicatePostAlias,
                    "alias duplicates an earlier alias",
                ),
                RouteConflict::AliasMatchesSlug => (
                    ContentValidationCode::AliasMatchesSlug,
                    "alias matches its post's canonical slug",
                ),
                RouteConflict::DuplicateRoute => (
                    ContentValidationCode::DuplicatePostRoute,
                    "post route conflicts with an earlier canonical slug or alias",
                ),
            };
            diagnostics.push(
                ContentValidationError::new(
                    duplicate.post.path.clone(),
                    duplicate.field.clone(),
                    code,
                    message,
                )
                .with_related(ValidationLocation::new(
                    anchor.post.path.clone(),
                    super::FieldPath::new(anchor.field.clone()),
                )),
            );
        }
    }
}

#[derive(Clone, Copy)]
enum OptionalField<Value> {
    Missing,
    Valid(Value),
    Invalid,
}

impl<Value> OptionalField<Value> {
    fn into_option(self) -> Option<Value> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Missing | Self::Invalid => None,
        }
    }
}

fn take_required_table(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Table> {
    match table.remove(key) {
        Some(Value::Table(value)) => Some(value),
        Some(value) => {
            invalid_type(path, field, "table", &value, diagnostics);
            None
        }
        None => {
            required_field(
                path,
                field,
                diagnostics,
                ContentValidationCode::RequiredFieldMissing,
            );
            None
        }
    }
}

fn take_optional_table(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> OptionalField<Table> {
    match table.remove(key) {
        Some(Value::Table(value)) => OptionalField::Valid(value),
        Some(value) => {
            invalid_type(path, field, "table", &value, diagnostics);
            OptionalField::Invalid
        }
        None => OptionalField::Missing,
    }
}

fn take_required_string(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<String> {
    match table.remove(key) {
        Some(Value::String(value)) => Some(value),
        Some(value) => {
            invalid_type(path, field, "string", &value, diagnostics);
            None
        }
        None => {
            required_field(
                path,
                field,
                diagnostics,
                ContentValidationCode::RequiredFieldMissing,
            );
            None
        }
    }
}

fn take_optional_string(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<String> {
    match table.remove(key) {
        Some(Value::String(value)) => Some(value),
        Some(value) => {
            invalid_type(path, field, "string", &value, diagnostics);
            None
        }
        None => None,
    }
}

fn take_optional_string_array(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Vec<(usize, String)> {
    match table.remove(key) {
        Some(Value::Array(values)) => values
            .into_iter()
            .enumerate()
            .filter_map(|(index, value)| match value {
                Value::String(value) => Some((index, value)),
                value => {
                    invalid_type(
                        path,
                        &indexed_field(field, index),
                        "string",
                        &value,
                        diagnostics,
                    );
                    None
                }
            })
            .collect(),
        Some(value) => {
            invalid_type(path, field, "array", &value, diagnostics);
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn take_optional_bool(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> OptionalField<bool> {
    match table.remove(key) {
        Some(Value::Boolean(value)) => OptionalField::Valid(value),
        Some(value) => {
            invalid_type(path, field, "boolean", &value, diagnostics);
            OptionalField::Invalid
        }
        None => OptionalField::Missing,
    }
}

fn take_required_bool(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
    missing_code: ContentValidationCode,
) -> Option<bool> {
    match table.remove(key) {
        Some(Value::Boolean(value)) => Some(value),
        Some(value) => {
            invalid_type(path, field, "boolean", &value, diagnostics);
            None
        }
        None => {
            required_field(path, field, diagnostics, missing_code);
            None
        }
    }
}

fn take_required_datetime(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<OffsetDateTime> {
    match table.remove(key) {
        Some(value) => parse_datetime_value(value, field, path, diagnostics),
        None => {
            required_field(
                path,
                field,
                diagnostics,
                ContentValidationCode::RequiredFieldMissing,
            );
            None
        }
    }
}

fn take_optional_datetime(
    table: &mut Table,
    key: &str,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> OptionalField<OffsetDateTime> {
    match table.remove(key) {
        Some(value) => parse_datetime_value(value, field, path, diagnostics)
            .map_or(OptionalField::Invalid, OptionalField::Valid),
        None => OptionalField::Missing,
    }
}

fn parse_datetime_value(
    value: Value,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<OffsetDateTime> {
    let Value::Datetime(datetime) = value else {
        invalid_type(path, field, "TOML offset datetime", &value, diagnostics);
        return None;
    };
    if datetime.date.is_none() || datetime.time.is_none() || datetime.offset.is_none() {
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            field,
            ContentValidationCode::DatetimeOffsetRequired,
            "timestamp must include a date, time, and UTC offset",
        ));
        return None;
    }
    match OffsetDateTime::parse(&datetime.to_string(), &Rfc3339) {
        Ok(value) => Some(value),
        Err(_) => {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                field,
                ContentValidationCode::DatetimeInvalid,
                "timestamp is not a supported RFC 3339 offset datetime",
            ));
            None
        }
    }
}

fn parse_plain_text<Value>(
    raw: String,
    constructor: impl FnOnce(String) -> Result<Value, PlainTextError>,
    field: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) -> Option<Value> {
    match constructor(raw) {
        Ok(value) => Some(value),
        Err(PlainTextError::Empty) => {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                field,
                ContentValidationCode::TextEmpty,
                "text value must not be empty",
            ));
            None
        }
        Err(PlainTextError::ContainsControl) => {
            diagnostics.push(ContentValidationError::new(
                path.clone(),
                field,
                ContentValidationCode::TextContainsControl,
                "text value must not contain control characters or newlines",
            ));
            None
        }
    }
}

fn reject_unknown_fields(
    table: Table,
    prefix: &str,
    path: &LogicalContentPath,
    diagnostics: &mut DiagnosticCollector,
) {
    let mut keys: Vec<_> = table.into_iter().map(|(key, _)| key).collect();
    keys.sort();
    for key in keys {
        let field = if prefix.is_empty() {
            key
        } else {
            format!("{prefix}.{key}")
        };
        diagnostics.push(ContentValidationError::new(
            path.clone(),
            field,
            ContentValidationCode::UnknownField,
            "field is not part of the v1 content contract",
        ));
    }
}

fn required_field(
    path: &LogicalContentPath,
    field: &str,
    diagnostics: &mut DiagnosticCollector,
    code: ContentValidationCode,
) {
    diagnostics.push(ContentValidationError::new(
        path.clone(),
        field,
        code,
        "required field is missing",
    ));
}

fn invalid_type(
    path: &LogicalContentPath,
    field: &str,
    expected: &str,
    actual: &Value,
    diagnostics: &mut DiagnosticCollector,
) {
    diagnostics.push(ContentValidationError::new(
        path.clone(),
        field,
        ContentValidationCode::InvalidFieldType,
        format!("expected {expected}, found {}", value_kind(actual)),
    ));
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "boolean",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::Table(_) => "table",
    }
}

fn indexed_field(field: &str, index: usize) -> String {
    format!("{field}[{index}]")
}

fn invariant_error(path: LogicalContentPath, message: &str) -> ContentValidationError {
    ContentValidationError::new(
        path,
        "$document",
        ContentValidationCode::InternalValidationInvariant,
        message,
    )
}

fn single_invariant_error(path: LogicalContentPath) -> ContentValidationErrors {
    let mut diagnostics = DiagnosticCollector::default();
    diagnostics.push(invariant_error(
        path,
        "content validation invariant failed while producing the final model",
    ));
    diagnostics.finish()
}
