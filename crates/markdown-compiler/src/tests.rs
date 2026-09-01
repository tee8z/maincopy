use time::UtcOffset;

use crate::{
    DefaultPostTipPolicy, PostAlias, PostSlug, PostTag, PublicationBaseUrl, PublicationBaseUrlError,
};

use super::*;

const FULL_PUBLICATION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/publication/full.toml"
));
const MINIMAL_PUBLICATION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/publication/minimal.toml"
));
const FULL_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/posts/full-valid.md"
));
const MINIMAL_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/posts/minimal-valid.md"
));
const CRLF_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/posts/crlf-valid.md"
));
const UNKNOWN_FIELD_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/invalid/unknown-field.md"
));
const PUBLISHED_AT_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/invalid/published-at.md"
));
const MALFORMED_DELIMITER_POST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/content/invalid/malformed-delimiter.md"
));

fn validate<'source>(
    publication: &'source str,
    posts: &[(&'source str, &'source str)],
) -> Result<ValidatedContent, ContentValidationErrors> {
    validate_content(
        PublicationSource::new("publication.toml", publication),
        posts
            .iter()
            .map(|(path, contents)| PostSource::in_posts(*path, contents)),
    )
}

fn error_contract(
    result: Result<ValidatedContent, ContentValidationErrors>,
) -> Vec<(String, String, ContentValidationCode)> {
    result
        .expect_err("fixture must fail validation")
        .into_errors()
        .into_iter()
        .map(|error| {
            (
                error.path.as_str().to_owned(),
                error.field.as_str().to_owned(),
                error.code,
            )
        })
        .collect()
}

fn post(id: &str, slug: &str, extra: &str) -> String {
    format!(
        "+++\nid = \"{id}\"\ntitle = \"Post {slug}\"\nslug = \"{slug}\"\nauthored_at = 2026-08-29T12:00:00Z\ndescription = \"Description for {slug}.\"\n{extra}+++\n# {slug}\n"
    )
}

#[test]
fn standalone_post_validation_uses_a_label_without_enforcing_tree_placement() {
    let document =
        validate_post_document("/tmp/Authored Post.MD", MINIMAL_POST, PostCollection::Posts)
            .unwrap();

    assert_eq!(document.path.as_str(), "/tmp/Authored Post.MD");
    assert_eq!(document.metadata.slug.as_str(), "a-minimal-post");
}

#[test]
fn standalone_post_validation_keeps_same_post_route_checks() {
    let source = MINIMAL_POST.replace(
        "description = \"Only the required post fields are present.\"",
        "description = \"Only the required post fields are present.\"\n\
         aliases = [\"a-minimal-post\"]",
    );
    let errors = validate_post_document("post.md", &source, PostCollection::Posts).unwrap_err();

    assert_eq!(errors.errors().len(), 1);
    let error = &errors.errors()[0];
    assert_eq!(error.code, ContentValidationCode::AliasMatchesSlug);
    assert_eq!(error.path.as_str(), "post.md");
    assert_eq!(error.field.as_str(), "aliases[0]");
    let related = error.related.as_ref().unwrap();
    assert_eq!(related.path.as_str(), "post.md");
    assert_eq!(related.field.as_str(), "slug");
}

#[test]
fn standalone_byte_validation_enforces_size_before_utf8() {
    let invalid_utf8 =
        validate_post_document_bytes("invalid.md", &[0xff], PostCollection::Posts).unwrap_err();
    assert_eq!(
        invalid_utf8.errors()[0].code,
        ContentValidationCode::ContentTextInvalidUtf8
    );

    let oversized = vec![0xff; crate::tree::DEFAULT_POST_BYTES as usize + 1];
    let too_large =
        validate_post_document_bytes("large.md", &oversized, PostCollection::Posts).unwrap_err();
    assert_eq!(
        too_large.errors()[0].code,
        ContentValidationCode::ContentFileTooLarge
    );
}

#[test]
fn checked_in_full_minimal_and_crlf_fixtures_form_one_valid_catalog() {
    assert!(
        CRLF_POST.contains("\r\n"),
        "fixture must contain real CRLF bytes"
    );
    let content = validate(
        FULL_PUBLICATION,
        &[
            ("posts/minimal.md", MINIMAL_POST),
            ("posts/full.md", FULL_POST),
            ("posts/crlf.md", CRLF_POST),
        ],
    )
    .unwrap();

    assert_eq!(content.posts.len(), 3);
    assert_eq!(content.posts[0].path.as_str(), "posts/crlf.md");
    assert_eq!(content.posts[1].path.as_str(), "posts/full.md");
    assert_eq!(content.posts[2].path.as_str(), "posts/minimal.md");

    let full = &content.posts[1];
    assert_eq!(
        full.metadata.title.as_str(),
        "SQLite Does Not Need a Network"
    );
    assert_eq!(
        full.metadata.authored_at.offset(),
        UtcOffset::from_hms(-4, 0, 0).unwrap()
    );
    assert_eq!(
        full.metadata
            .tags
            .iter()
            .map(PostTag::as_str)
            .collect::<Vec<_>>(),
        ["rust", "sqlite"]
    );
    assert_eq!(full.metadata.tips, PostTipPolicy::Enabled);
    assert_eq!(
        content.publication.site.favicon.as_ref().unwrap().as_str(),
        "https://cdn.example.com/site/favicon-v1.png"
    );
    assert_eq!(
        content.publication.assets.allowed_https_origins[0].as_str(),
        "https://cdn.example.com"
    );
    assert_eq!(
        full.metadata.image.as_ref().unwrap().as_str(),
        "https://cdn.example.com/posts/sqlite/cover-v1.webp"
    );
    assert!(full.markdown.as_str().starts_with("\n# SQLite"));

    let serialized = serde_json::to_value(&content).unwrap();
    assert_eq!(
        serialized["publication"]["site"]["favicon"],
        "https://cdn.example.com/site/favicon-v1.png"
    );
    assert_eq!(
        serialized["publication"]["assets"]["allowed_https_origins"][0],
        "https://cdn.example.com"
    );
    assert_eq!(
        serialized["posts"][1]["metadata"]["image"],
        "https://cdn.example.com/posts/sqlite/cover-v1.webp"
    );
    assert!(serialized["publication"].get("subscriptions").is_none());
    assert!(
        serialized["posts"][1]["metadata"]
            .get("distribution")
            .is_none()
    );
}

#[test]
fn minimal_documents_apply_every_documented_default() {
    let content = validate(MINIMAL_PUBLICATION, &[("posts/minimal.md", MINIMAL_POST)]).unwrap();
    let publication = &content.publication;
    assert_eq!(
        publication.site.base_url.as_str(),
        "https://minimal.example.test/"
    );
    assert_eq!(publication.tips, DefaultPostTipPolicy::Disabled);

    let post = &content.posts[0].metadata;
    assert_eq!(post.updated_at, None);
    assert!(post.tags.is_empty());
    assert!(post.aliases.is_empty());
    assert_eq!(post.draft, DraftStatus::Publishable);
    assert_eq!(post.tips, PostTipPolicy::InheritPublication);
}

#[test]
fn draft_collection_is_typed_and_cannot_be_overridden_publishable() {
    let drafted = validate_content(
        PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
        [PostSource::in_drafts("drafts/minimal.md", MINIMAL_POST)],
    )
    .unwrap();
    assert_eq!(drafted.posts[0].metadata.draft, DraftStatus::Draft);

    let explicit_true = MINIMAL_POST.replace(
        "description = \"Only the required post fields are present.\"",
        "description = \"Only the required post fields are present.\"\ndraft = true",
    );
    let drafted = validate_content(
        PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
        [PostSource::in_drafts("drafts/explicit.md", &explicit_true)],
    )
    .unwrap();
    assert_eq!(drafted.posts[0].metadata.draft, DraftStatus::Draft);

    let explicit_false = MINIMAL_POST.replace(
        "description = \"Only the required post fields are present.\"",
        "description = \"Only the required post fields are present.\"\ndraft = false",
    );
    let error = validate_content(
        PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
        [PostSource::in_drafts("drafts/conflict.md", &explicit_false)],
    )
    .unwrap_err();
    assert_eq!(
        error.errors()[0].code,
        ContentValidationCode::DraftDirectoryConflict
    );
}

#[test]
fn post_collection_and_logical_path_must_agree() {
    for source in [
        PostSource::in_posts("drafts/wrong.md", MINIMAL_POST),
        PostSource::in_drafts("posts/wrong.md", MINIMAL_POST),
    ] {
        let path = source.path.as_str().to_owned();
        assert_eq!(
            error_contract(validate_content(
                PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
                [source],
            )),
            [(
                path,
                "$path".to_owned(),
                ContentValidationCode::PostCollectionPathMismatch,
            )]
        );
    }
}

#[test]
fn direct_post_sources_enforce_portable_markdown_paths() {
    for path in [
        "posts/../drafts/x.md",
        "posts/./x.md",
        "posts//x.md",
        "posts\\x.md",
        "posts/%2e%2e/x.md",
        "posts/café.md",
        "/posts/x.md",
    ] {
        let errors = error_contract(validate_content(
            PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
            [PostSource::in_posts(path, MINIMAL_POST)],
        ));
        assert!(
            errors.iter().any(|(_, field, code)| {
                field == "$path" && *code == ContentValidationCode::InvalidLogicalContentPath
            }),
            "missing portable-path diagnostic for {path}: {errors:?}"
        );
    }

    for path in ["posts/readme.txt", "posts/upper.MD", "posts/no-extension"] {
        assert_eq!(
            error_contract(validate_content(
                PublicationSource::new("publication.toml", MINIMAL_PUBLICATION),
                [PostSource::in_posts(path, MINIMAL_POST)],
            )),
            [(
                path.to_owned(),
                "$path".to_owned(),
                ContentValidationCode::UnexpectedPostEntry,
            )]
        );
    }
}

#[test]
fn authored_plain_text_is_trimmed_and_offsets_are_preserved() {
    let publication = MINIMAL_PUBLICATION
        .replace("Minimal Publication", "  Minimal Publication  ")
        .replace("Minimal Author", "  Minimal Author  ")
        .replace(
            "Only the required publication fields are present.",
            "  Only the required publication fields are present.  ",
        );
    let post = MINIMAL_POST
        .replace("A Minimal Post", "  A Minimal Post  ")
        .replace(
            "Only the required post fields are present.",
            "  Only the required post fields are present.  ",
        )
        .replace("2026-08-29T12:00:00Z", "2026-08-29T12:00:00+03:00");
    let content = validate(&publication, &[("posts/post.md", &post)]).unwrap();
    assert_eq!(
        content.publication.site.title.as_str(),
        "Minimal Publication"
    );
    assert_eq!(content.publication.author.name.as_str(), "Minimal Author");
    assert_eq!(content.posts[0].metadata.title.as_str(), "A Minimal Post");
    assert_eq!(
        content.posts[0].metadata.authored_at.offset(),
        UtcOffset::from_hms(3, 0, 0).unwrap()
    );
}

#[test]
fn authored_order_is_preserved_and_authored_time_does_not_create_visibility() {
    let post = MINIMAL_POST
        .replace("2026-08-29T12:00:00Z", "2099-12-31T23:59:59-05:00")
        .replace(
            "description = \"Only the required post fields are present.\"",
            "description = \"Only the required post fields are present.\"\n\
             tags = [\"Second\", \"First\"]\n\
             aliases = [\"second-route\", \"first-route\"]",
        );
    let content = validate(MINIMAL_PUBLICATION, &[("posts/post.md", &post)]).unwrap();
    let metadata = &content.posts[0].metadata;

    assert_eq!(
        metadata
            .tags
            .iter()
            .map(PostTag::as_str)
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    assert_eq!(
        metadata
            .aliases
            .iter()
            .map(PostAlias::as_str)
            .collect::<Vec<_>>(),
        ["second-route", "first-route"]
    );
    assert_eq!(metadata.draft, DraftStatus::Publishable);

    let serialized = serde_json::to_value(metadata).unwrap();
    assert!(serialized.get("published_at").is_none());
    assert!(serialized.get("scheduled_for").is_none());
    assert!(serialized.get("visibility").is_none());
}

#[test]
fn base_url_accepts_only_an_absolute_https_root_origin() {
    assert_eq!(
        PublicationBaseUrl::parse("  https://example.com  ")
            .unwrap()
            .as_str(),
        "https://example.com/"
    );

    for invalid in [
        "https://exa\tmple.com",
        "https://example.com\\",
        "https:\\example.com",
    ] {
        assert_eq!(
            PublicationBaseUrl::parse(invalid),
            Err(PublicationBaseUrlError),
            "URL parser unexpectedly normalized {invalid:?}"
        );
    }

    for invalid in [
        "http://example.com/",
        "https://user@example.com/",
        "https://@example.com/",
        "HTTPS://@example.com/",
        "https://example.com/blog",
        "https://example.com/blog/..",
        "https://example.com/%2e",
        "https://example.com/./",
        "https://example.com//",
        "https://example.com/?query=yes",
        "https://example.com/#fragment",
        "/relative",
    ] {
        let publication = MINIMAL_PUBLICATION.replace("https://minimal.example.test", invalid);
        assert_eq!(
            error_contract(validate(&publication, &[("posts/post.md", MINIMAL_POST)])),
            [(
                "publication.toml".to_owned(),
                "site.base_url".to_owned(),
                ContentValidationCode::InvalidBaseUrl,
            )],
            "unexpected result for {invalid}"
        );
    }
}

#[test]
fn publication_requires_site_and_author_contracts_and_aggregates_fields() {
    let publication = "[site]\ntitle = 7\nbase_url = false\n\n[author]\nunknown = true\n";
    assert_eq!(
        error_contract(validate(publication, &[("posts/post.md", MINIMAL_POST)])),
        [
            (
                "publication.toml".to_owned(),
                "site.title".to_owned(),
                ContentValidationCode::InvalidFieldType,
            ),
            (
                "publication.toml".to_owned(),
                "site.base_url".to_owned(),
                ContentValidationCode::InvalidFieldType,
            ),
            (
                "publication.toml".to_owned(),
                "site.description".to_owned(),
                ContentValidationCode::RequiredFieldMissing,
            ),
            (
                "publication.toml".to_owned(),
                "author.name".to_owned(),
                ContentValidationCode::RequiredFieldMissing,
            ),
            (
                "publication.toml".to_owned(),
                "author.unknown".to_owned(),
                ContentValidationCode::UnknownField,
            ),
        ]
    );
}

#[test]
fn subscription_configuration_is_not_part_of_the_v1_content_contract() {
    for unsupported in [
        format!("{MINIMAL_PUBLICATION}\n[subscriptions]\nenabled = true\n"),
        format!("subscriptions = false\n{MINIMAL_PUBLICATION}"),
    ] {
        assert_eq!(
            error_contract(validate(&unsupported, &[("posts/post.md", MINIMAL_POST)])),
            [(
                "publication.toml".to_owned(),
                "subscriptions".to_owned(),
                ContentValidationCode::UnknownField,
            )]
        );
    }
}

#[test]
fn publication_default_and_post_override_are_independent_tip_policies() {
    let enabled_post = MINIMAL_POST.replace("description =", "tips = true\ndescription =");
    let content = validate(MINIMAL_PUBLICATION, &[("posts/post.md", &enabled_post)]).unwrap();
    assert_eq!(content.publication.tips, DefaultPostTipPolicy::Disabled);
    assert_eq!(content.posts[0].metadata.tips, PostTipPolicy::Enabled);

    let enabled_publication = format!("{MINIMAL_PUBLICATION}\n[tips]\nenabled = true\n");
    let content = validate(&enabled_publication, &[("posts/post.md", MINIMAL_POST)]).unwrap();
    assert_eq!(content.publication.tips, DefaultPostTipPolicy::Enabled);
    assert_eq!(
        content.posts[0].metadata.tips,
        PostTipPolicy::InheritPublication
    );
}

#[test]
fn authored_tip_amounts_are_rejected_as_unknown_fields() {
    let publication = format!(
        "{MINIMAL_PUBLICATION}\n[tips]\nenabled = true\nminimum_sats = 1\nmaximum_sats = 100\n"
    );
    assert_eq!(
        error_contract(validate(&publication, &[("posts/post.md", MINIMAL_POST)],)),
        [
            (
                "publication.toml".to_owned(),
                "tips.maximum_sats".to_owned(),
                ContentValidationCode::UnknownField,
            ),
            (
                "publication.toml".to_owned(),
                "tips.minimum_sats".to_owned(),
                ContentValidationCode::UnknownField,
            ),
        ]
    );

    let post_with_amounts = MINIMAL_POST.replace(
        "description =",
        "minimum_sats = 1\nmaximum_sats = 100\ndescription =",
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/post.md", &post_with_amounts)],
        )),
        [
            (
                "posts/post.md".to_owned(),
                "maximum_sats".to_owned(),
                ContentValidationCode::UnknownField,
            ),
            (
                "posts/post.md".to_owned(),
                "minimum_sats".to_owned(),
                ContentValidationCode::UnknownField,
            ),
        ]
    );
}

#[test]
fn checked_in_invalid_fixtures_have_specific_contract_codes() {
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/unknown.md", UNKNOWN_FIELD_POST)]
        )),
        [(
            "posts/unknown.md".to_owned(),
            "raw_html".to_owned(),
            ContentValidationCode::UnknownField,
        )]
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/published.md", PUBLISHED_AT_POST)]
        )),
        [(
            "posts/published.md".to_owned(),
            "published_at".to_owned(),
            ContentValidationCode::PublishedAtUnsupported,
        )]
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/malformed.md", MALFORMED_DELIMITER_POST)]
        )),
        [(
            "posts/malformed.md".to_owned(),
            "$frontmatter".to_owned(),
            ContentValidationCode::FrontmatterClosingDelimiterMissing,
        )]
    );
}

#[test]
fn delimiter_grammar_is_exact_but_accepts_lf_crlf_and_empty_bodies() {
    let valid_empty_body = "+++\n\
id = \"77777777-7777-4777-8777-777777777777\"\n\
title = \"Empty Body\"\n\
slug = \"empty-body\"\n\
authored_at = 2026-08-29T12:00:00Z\n\
description = \"The closing delimiter is the end of the file.\"\n\
+++";
    validate(
        MINIMAL_PUBLICATION,
        &[("posts/empty-body.md", valid_empty_body)],
    )
    .unwrap();

    for (document, code) in [
        (
            MINIMAL_POST.replacen("+++", "+++ ", 1),
            ContentValidationCode::FrontmatterOpeningDelimiterMalformed,
        ),
        (
            format!("\n{MINIMAL_POST}"),
            ContentValidationCode::FrontmatterOpeningDelimiterMissing,
        ),
        (
            MINIMAL_POST.replacen("\n+++\n", "\n+++ \n", 1),
            ContentValidationCode::FrontmatterClosingDelimiterMalformed,
        ),
    ] {
        assert_eq!(
            error_contract(validate(
                MINIMAL_PUBLICATION,
                &[("posts/delimiter.md", &document)]
            ))[0]
                .2,
            code
        );
    }
}

#[test]
fn all_required_post_fields_and_wrong_types_are_aggregated_in_field_order() {
    let invalid = "+++\nid = 7\ntitle = false\nslug = []\nauthored_at = 2026-08-29\ndescription = {}\ntags = [\"good\", 7]\n+++\n";
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/invalid.md", invalid)]
        )),
        [
            (
                "posts/invalid.md".to_owned(),
                "id".to_owned(),
                ContentValidationCode::InvalidFieldType
            ),
            (
                "posts/invalid.md".to_owned(),
                "title".to_owned(),
                ContentValidationCode::InvalidFieldType
            ),
            (
                "posts/invalid.md".to_owned(),
                "slug".to_owned(),
                ContentValidationCode::InvalidFieldType
            ),
            (
                "posts/invalid.md".to_owned(),
                "authored_at".to_owned(),
                ContentValidationCode::DatetimeOffsetRequired
            ),
            (
                "posts/invalid.md".to_owned(),
                "description".to_owned(),
                ContentValidationCode::InvalidFieldType
            ),
            (
                "posts/invalid.md".to_owned(),
                "tags[1]".to_owned(),
                ContentValidationCode::InvalidFieldType
            ),
        ]
    );
}

#[test]
fn canonical_uuid_slug_tag_and_alias_rules_are_enforced() {
    let canonical_id = "4f054633-2d09-4b05-97d0-c6f0011a5199";
    assert_eq!(
        serde_json::to_value(PostId::parse(canonical_id).unwrap()).unwrap(),
        serde_json::json!(canonical_id)
    );

    for invalid_id in [
        "4F054633-2D09-4B05-97D0-C6F0011A5199",
        "4f0546332d094b0597d0c6f0011a5199",
        "not-a-uuid",
    ] {
        let invalid = MINIMAL_POST.replace("6a115f8e-7ef4-4f93-9f4d-b2534a1357fd", invalid_id);
        assert_eq!(
            error_contract(validate(
                MINIMAL_PUBLICATION,
                &[("posts/invalid.md", &invalid)]
            ))[0]
                .2,
            ContentValidationCode::InvalidPostId
        );
    }

    for invalid_slug in [
        "Uppercase",
        "under_score",
        "-leading",
        "trailing-",
        "two--words",
        "café",
    ] {
        let invalid = MINIMAL_POST.replace("a-minimal-post", invalid_slug);
        assert_eq!(
            error_contract(validate(
                MINIMAL_PUBLICATION,
                &[("posts/invalid.md", &invalid)]
            ))[0]
                .2,
            ContentValidationCode::InvalidPostSlug
        );
    }

    let maximum_route_value = "a".repeat(1024);
    assert!(PostSlug::parse(&maximum_route_value).is_ok());
    assert!(PostAlias::parse(&maximum_route_value).is_ok());
    assert!(PostTag::parse(&maximum_route_value).is_ok());
    let oversized_route_value = "a".repeat(1025);
    assert!(PostSlug::parse(&oversized_route_value).is_err());
    assert!(PostAlias::parse(&oversized_route_value).is_err());
    assert!(PostTag::parse(&oversized_route_value).is_err());

    let tags = MINIMAL_POST.replace(
        "description = \"Only the required post fields are present.\"",
        "description = \"Only the required post fields are present.\"\ntags = [\" Rust \" , \"RUST\", \"C++\"]",
    );
    let codes: Vec<_> = error_contract(validate(MINIMAL_PUBLICATION, &[("posts/tags.md", &tags)]))
        .into_iter()
        .map(|(_, field, code)| (field, code))
        .collect();
    assert_eq!(
        codes,
        [
            ("tags[1]".to_owned(), ContentValidationCode::DuplicateTag),
            ("tags[2]".to_owned(), ContentValidationCode::InvalidPostTag),
        ]
    );
}

#[test]
fn timestamps_require_native_offset_datetimes_and_compare_instants() {
    let local = MINIMAL_POST.replace("2026-08-29T12:00:00Z", "2026-08-29T12:00:00");
    assert_eq!(
        error_contract(validate(MINIMAL_PUBLICATION, &[("posts/local.md", &local)]))[0].2,
        ContentValidationCode::DatetimeOffsetRequired
    );
    let quoted = MINIMAL_POST.replace("2026-08-29T12:00:00Z", "\"2026-08-29T12:00:00Z\"");
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/quoted.md", &quoted)]
        ))[0]
            .2,
        ContentValidationCode::InvalidFieldType
    );
    let earlier = MINIMAL_POST.replace(
        "description =",
        "updated_at = 2026-08-29T12:59:59+01:00\ndescription =",
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/earlier.md", &earlier)]
        ))[0]
            .2,
        ContentValidationCode::UpdatedAtBeforeAuthoredAt
    );
    let equal = MINIMAL_POST.replace(
        "description =",
        "updated_at = 2026-08-29T13:00:00+01:00\ndescription =",
    );
    validate(MINIMAL_PUBLICATION, &[("posts/equal.md", &equal)]).unwrap();
}

#[test]
fn duplicate_id_and_route_checks_use_partial_candidates() {
    let first = post(
        "11111111-1111-4111-8111-111111111111",
        "first",
        "aliases = [\"shared-route\"]\n",
    );
    let second = post("11111111-1111-4111-8111-111111111111", "shared-route", "")
        .replace("title = \"Post shared-route\"", "title = \"\"");
    let errors = error_contract(validate(
        MINIMAL_PUBLICATION,
        &[("posts/z.md", &second), ("posts/a.md", &first)],
    ));
    assert!(
        errors
            .iter()
            .any(|(_, _, code)| *code == ContentValidationCode::TextEmpty)
    );
    assert!(
        errors
            .iter()
            .any(|(_, _, code)| *code == ContentValidationCode::DuplicatePostId)
    );
    assert!(
        errors
            .iter()
            .any(|(_, _, code)| *code == ContentValidationCode::DuplicatePostRoute)
    );
}

#[test]
fn aliases_share_one_global_route_namespace() {
    let first = post(
        "11111111-1111-4111-8111-111111111111",
        "first",
        "aliases = [\"first\", \"shared\", \"shared\"]\n",
    );
    let second = post("22222222-2222-4222-8222-222222222222", "shared", "");
    let codes: Vec<_> = error_contract(validate(
        MINIMAL_PUBLICATION,
        &[("posts/first.md", &first), ("posts/second.md", &second)],
    ))
    .into_iter()
    .map(|(_, _, code)| code)
    .collect();
    assert!(codes.contains(&ContentValidationCode::AliasMatchesSlug));
    assert!(codes.contains(&ContentValidationCode::DuplicatePostAlias));
    assert!(codes.contains(&ContentValidationCode::DuplicatePostRoute));
}

#[test]
fn distribution_configuration_is_not_part_of_the_v1_content_contract() {
    for table in ["x", "nostr"] {
        let unsupported = MINIMAL_POST.replace(
            "+++\n\n# A Minimal Post",
            &format!("\n[distribution.{table}]\nenabled = true\n+++\n\n# A Minimal Post"),
        );
        assert_eq!(
            error_contract(validate(
                MINIMAL_PUBLICATION,
                &[("posts/target.md", &unsupported)]
            )),
            [(
                "posts/target.md".to_owned(),
                "distribution".to_owned(),
                ContentValidationCode::UnknownField,
            )]
        );
    }
}

#[test]
fn error_order_is_stable_across_input_permutations() {
    let first = post(
        "11111111-1111-4111-8111-111111111111",
        "same",
        "aliases = [\"same\"]\n",
    );
    let second = post("11111111-1111-4111-8111-111111111111", "same", "");
    let forward = error_contract(validate(
        MINIMAL_PUBLICATION,
        &[("posts/b.md", &second), ("posts/a.md", &first)],
    ));
    let reverse = error_contract(validate(
        MINIMAL_PUBLICATION,
        &[("posts/a.md", &first), ("posts/b.md", &second)],
    ));
    assert_eq!(forward, reverse);

    let repeated = error_contract(validate(
        MINIMAL_PUBLICATION,
        &[("posts/b.md", &second), ("posts/a.md", &first)],
    ));
    assert_eq!(forward, repeated);
}

#[test]
fn enum_and_validation_code_wire_names_are_stable() {
    for (value, expected) in [
        (
            serde_json::to_value(DraftStatus::Publishable).unwrap(),
            "publishable",
        ),
        (serde_json::to_value(DraftStatus::Draft).unwrap(), "draft"),
        (
            serde_json::to_value(PostTipPolicy::InheritPublication).unwrap(),
            "inherit_publication",
        ),
        (
            serde_json::to_value(PostTipPolicy::Enabled).unwrap(),
            "enabled",
        ),
        (
            serde_json::to_value(PostTipPolicy::Disabled).unwrap(),
            "disabled",
        ),
        (
            serde_json::to_value(DefaultPostTipPolicy::Enabled).unwrap(),
            "enabled",
        ),
        (
            serde_json::to_value(DefaultPostTipPolicy::Disabled).unwrap(),
            "disabled",
        ),
        (
            serde_json::to_value(PostCollection::Posts).unwrap(),
            "posts",
        ),
        (
            serde_json::to_value(PostCollection::Drafts).unwrap(),
            "drafts",
        ),
    ] {
        assert_eq!(value, serde_json::json!(expected));
    }

    for (error, expected) in [
        (LogicalTreePathError::Empty, "empty"),
        (LogicalTreePathError::Absolute, "absolute"),
        (
            LogicalTreePathError::UnsupportedComponent,
            "unsupported_component",
        ),
        (LogicalTreePathError::EncodedTraversal, "encoded_traversal"),
        (LogicalTreePathError::TooLong, "too_long"),
        (
            LogicalTreePathError::WrongAssetNamespace,
            "wrong_asset_namespace",
        ),
    ] {
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!(expected)
        );
    }

    assert_eq!(
        serde_json::to_value(ContentTreeLimits::default()).unwrap(),
        serde_json::json!({
            "publication_file_bytes": 262_144,
            "post_file_bytes": 4_194_304,
            "asset_file_bytes": 33_554_432,
            "total_tree_bytes": 268_435_456,
            "entries": 10_000,
            "depth": 16,
            "path_bytes": 1_024,
        })
    );

    let codes = [
        (
            ContentValidationCode::ContentPlatformUnsupported,
            "content_platform_unsupported",
        ),
        (
            ContentValidationCode::ContentRootUnavailable,
            "content_root_unavailable",
        ),
        (
            ContentValidationCode::PublicationFileMissing,
            "publication_file_missing",
        ),
        (
            ContentValidationCode::ContentNamespaceInvalid,
            "content_namespace_invalid",
        ),
        (
            ContentValidationCode::ContentEntryUnreadable,
            "content_entry_unreadable",
        ),
        (
            ContentValidationCode::UnsupportedFilenameEncoding,
            "unsupported_filename_encoding",
        ),
        (
            ContentValidationCode::InvalidLogicalContentPath,
            "invalid_logical_content_path",
        ),
        (
            ContentValidationCode::UnexpectedPostEntry,
            "unexpected_post_entry",
        ),
        (
            ContentValidationCode::ContentSymlinkUnsupported,
            "content_symlink_unsupported",
        ),
        (
            ContentValidationCode::UnsupportedContentEntryKind,
            "unsupported_content_entry_kind",
        ),
        (
            ContentValidationCode::ContentTextInvalidUtf8,
            "content_text_invalid_utf8",
        ),
        (
            ContentValidationCode::AuthoredSvgUnsupported,
            "authored_svg_unsupported",
        ),
        (
            ContentValidationCode::ContentFileTooLarge,
            "content_file_too_large",
        ),
        (
            ContentValidationCode::ContentTreeTooLarge,
            "content_tree_too_large",
        ),
        (
            ContentValidationCode::ContentEntryLimitExceeded,
            "content_entry_limit_exceeded",
        ),
        (
            ContentValidationCode::ContentDepthLimitExceeded,
            "content_depth_limit_exceeded",
        ),
        (
            ContentValidationCode::ContentPathTooLong,
            "content_path_too_long",
        ),
        (
            ContentValidationCode::DuplicateLogicalAssetPath,
            "duplicate_logical_asset_path",
        ),
        (
            ContentValidationCode::LogicalPathCaseCollision,
            "logical_path_case_collision",
        ),
        (
            ContentValidationCode::ContentEntryChanged,
            "content_entry_changed",
        ),
        (
            ContentValidationCode::PublicationTomlInvalid,
            "publication_toml_invalid",
        ),
        (
            ContentValidationCode::FrontmatterOpeningDelimiterMissing,
            "frontmatter_opening_delimiter_missing",
        ),
        (
            ContentValidationCode::FrontmatterOpeningDelimiterMalformed,
            "frontmatter_opening_delimiter_malformed",
        ),
        (
            ContentValidationCode::FrontmatterClosingDelimiterMissing,
            "frontmatter_closing_delimiter_missing",
        ),
        (
            ContentValidationCode::FrontmatterClosingDelimiterMalformed,
            "frontmatter_closing_delimiter_malformed",
        ),
        (
            ContentValidationCode::FrontmatterTomlInvalid,
            "frontmatter_toml_invalid",
        ),
        (
            ContentValidationCode::RequiredFieldMissing,
            "required_field_missing",
        ),
        (
            ContentValidationCode::InvalidFieldType,
            "invalid_field_type",
        ),
        (ContentValidationCode::UnknownField, "unknown_field"),
        (
            ContentValidationCode::PublishedAtUnsupported,
            "published_at_unsupported",
        ),
        (ContentValidationCode::TextEmpty, "text_empty"),
        (
            ContentValidationCode::TextContainsControl,
            "text_contains_control",
        ),
        (ContentValidationCode::InvalidBaseUrl, "invalid_base_url"),
        (ContentValidationCode::InvalidPostId, "invalid_post_id"),
        (ContentValidationCode::InvalidPostSlug, "invalid_post_slug"),
        (ContentValidationCode::InvalidPostTag, "invalid_post_tag"),
        (
            ContentValidationCode::InvalidPostAlias,
            "invalid_post_alias",
        ),
        (
            ContentValidationCode::DatetimeOffsetRequired,
            "datetime_offset_required",
        ),
        (ContentValidationCode::DatetimeInvalid, "datetime_invalid"),
        (
            ContentValidationCode::UpdatedAtBeforeAuthoredAt,
            "updated_at_before_authored_at",
        ),
        (ContentValidationCode::DuplicateTag, "duplicate_tag"),
        (
            ContentValidationCode::AliasMatchesSlug,
            "alias_matches_slug",
        ),
        (ContentValidationCode::DuplicatePostId, "duplicate_post_id"),
        (
            ContentValidationCode::DuplicatePostSlug,
            "duplicate_post_slug",
        ),
        (
            ContentValidationCode::DuplicatePostAlias,
            "duplicate_post_alias",
        ),
        (
            ContentValidationCode::DuplicatePostRoute,
            "duplicate_post_route",
        ),
        (
            ContentValidationCode::DraftDirectoryConflict,
            "draft_directory_conflict",
        ),
        (
            ContentValidationCode::PostCollectionPathMismatch,
            "post_collection_path_mismatch",
        ),
        (
            ContentValidationCode::InternalValidationInvariant,
            "internal_validation_invariant",
        ),
    ];
    for (code, expected) in codes {
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            serde_json::json!(expected)
        );
    }
}
