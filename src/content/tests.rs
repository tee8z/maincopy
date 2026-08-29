use time::UtcOffset;

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
            .map(|(path, contents)| PostSource::new(*path, contents)),
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
                error.path().as_str().to_owned(),
                error.field().as_str().to_owned(),
                error.code(),
            )
        })
        .collect()
}

fn post(id: &str, slug: &str, extra: &str) -> String {
    format!(
        "+++\nid = \"{id}\"\ntitle = \"Post {slug}\"\nslug = \"{slug}\"\nauthored_at = 2026-08-29T12:00:00Z\ndescription = \"Description for {slug}.\"\n{extra}+++\n# {slug}\n"
    )
}

fn configured_disabled_tips_publication() -> String {
    format!(
        "{MINIMAL_PUBLICATION}\n[tips]\nenabled = false\nminimum_sats = 10\nmaximum_sats = 1000\n"
    )
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

    assert_eq!(content.posts().len(), 3);
    assert_eq!(content.posts()[0].path().as_str(), "posts/crlf.md");
    assert_eq!(content.posts()[1].path().as_str(), "posts/full.md");
    assert_eq!(content.posts()[2].path().as_str(), "posts/minimal.md");

    let full = &content.posts()[1];
    assert_eq!(
        full.metadata().title().as_str(),
        "SQLite Does Not Need a Network"
    );
    assert_eq!(
        full.metadata().authored_at().offset(),
        UtcOffset::from_hms(-4, 0, 0).unwrap()
    );
    assert_eq!(
        full.metadata()
            .tags()
            .iter()
            .map(PostTag::as_str)
            .collect::<Vec<_>>(),
        ["rust", "sqlite"]
    );
    assert_eq!(full.metadata().tips(), PostTipPolicy::Enabled);
    assert_eq!(
        full.metadata().distribution().x().mode(),
        DistributionMode::Enabled
    );
    assert_eq!(
        content.publication().site().favicon().unwrap().as_str(),
        "https://cdn.example.com/site/favicon-v1.png"
    );
    assert_eq!(
        content.publication().assets().allowed_https_origins()[0].as_str(),
        "https://cdn.example.com"
    );
    assert_eq!(
        full.metadata().image().unwrap().as_str(),
        "https://cdn.example.com/posts/sqlite/cover-v1.webp"
    );
    assert!(full.markdown().as_str().starts_with("\n# SQLite"));

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
}

#[test]
fn minimal_documents_apply_every_documented_default() {
    let content = validate(MINIMAL_PUBLICATION, &[("posts/minimal.md", MINIMAL_POST)]).unwrap();
    let publication = content.publication();
    assert_eq!(
        publication.site().base_url().as_str(),
        "https://minimal.example.test/"
    );
    assert_eq!(publication.subscriptions(), &SubscriptionSettings::Disabled);
    assert_eq!(publication.tips(), PublicationTipSettings::Unconfigured);
    assert_eq!(publication.renderer(), RendererSettings::baseline());

    let post = content.posts()[0].metadata();
    assert_eq!(post.updated_at(), None);
    assert!(post.tags().is_empty());
    assert!(post.aliases().is_empty());
    assert_eq!(post.draft(), DraftStatus::Publishable);
    assert_eq!(post.tips(), PostTipPolicy::InheritPublication);
    assert_eq!(post.distribution().x().mode(), DistributionMode::Disabled);
    assert!(post.distribution().x().copy().is_none());
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
        content.publication().site().title().as_str(),
        "Minimal Publication"
    );
    assert_eq!(
        content.publication().author().name().as_str(),
        "Minimal Author"
    );
    assert_eq!(
        content.posts()[0].metadata().title().as_str(),
        "A Minimal Post"
    );
    assert_eq!(
        content.posts()[0].metadata().authored_at().offset(),
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
    let metadata = content.posts()[0].metadata();

    assert_eq!(
        metadata
            .tags()
            .iter()
            .map(PostTag::as_str)
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    assert_eq!(
        metadata
            .aliases()
            .iter()
            .map(PostAlias::as_str)
            .collect::<Vec<_>>(),
        ["second-route", "first-route"]
    );
    assert_eq!(metadata.draft(), DraftStatus::Publishable);

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
fn subscriptions_are_typed_and_enabled_requires_a_revision() {
    let missing = format!("{MINIMAL_PUBLICATION}\n[subscriptions]\nenabled = true\n");
    assert_eq!(
        error_contract(validate(&missing, &[("posts/post.md", MINIMAL_POST)])),
        [(
            "publication.toml".to_owned(),
            "subscriptions.privacy_policy_revision".to_owned(),
            ContentValidationCode::SubscriptionPrivacyRevisionRequired,
        )]
    );

    let disabled_with_revision = format!(
        "{MINIMAL_PUBLICATION}\n[subscriptions]\nenabled = false\nprivacy_policy_revision = \"old\"\n"
    );
    let content = validate(&disabled_with_revision, &[("posts/post.md", MINIMAL_POST)]).unwrap();
    assert_eq!(
        content.publication().subscriptions(),
        &SubscriptionSettings::Disabled
    );
}

#[test]
fn post_tip_override_requires_a_configured_range() {
    let enabled_post = MINIMAL_POST.replace("description =", "tips = true\ndescription =");
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/post.md", &enabled_post)]
        )),
        [(
            "posts/post.md".to_owned(),
            "tips".to_owned(),
            ContentValidationCode::PostTipsUnconfigured,
        )]
    );

    let publication = configured_disabled_tips_publication();
    let content = validate(&publication, &[("posts/post.md", &enabled_post)]).unwrap();
    assert_eq!(
        content.publication().tips().default_policy(),
        DefaultPostTipPolicy::Disabled
    );
    assert!(content.publication().tips().range().is_some());
    assert_eq!(content.posts()[0].metadata().tips(), PostTipPolicy::Enabled);
}

#[test]
fn tip_ranges_require_both_positive_ordered_bounds() {
    for (body, expected_field, expected_code) in [
        (
            "enabled = true\nmaximum_sats = 100\n",
            "tips.minimum_sats",
            ContentValidationCode::TipRangeRequired,
        ),
        (
            "enabled = true\nminimum_sats = 0\nmaximum_sats = 100\n",
            "tips.minimum_sats",
            ContentValidationCode::TipAmountInvalid,
        ),
        (
            "enabled = true\nminimum_sats = 101\nmaximum_sats = 100\n",
            "tips.maximum_sats",
            ContentValidationCode::TipRangeInvalid,
        ),
    ] {
        let publication = format!("{MINIMAL_PUBLICATION}\n[tips]\n{body}");
        assert_eq!(
            error_contract(validate(&publication, &[("posts/post.md", MINIMAL_POST)])),
            [(
                "publication.toml".to_owned(),
                expected_field.to_owned(),
                expected_code,
            )]
        );
    }
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
fn distribution_is_closed_to_x_and_requires_an_explicit_mode() {
    let missing_enabled = MINIMAL_POST.replace(
        "+++\n\n# A Minimal Post",
        "\n[distribution.x]\ntext = \"copy\"\n+++\n\n# A Minimal Post",
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/x.md", &missing_enabled)]
        ))[0]
            .2,
        ContentValidationCode::DistributionEnabledRequired
    );

    let unknown = MINIMAL_POST.replace(
        "+++\n\n# A Minimal Post",
        "\n[distribution.nostr]\nenabled = true\n+++\n\n# A Minimal Post",
    );
    assert_eq!(
        error_contract(validate(
            MINIMAL_PUBLICATION,
            &[("posts/target.md", &unknown)]
        ))[0]
            .2,
        ContentValidationCode::UnknownField
    );
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
            serde_json::to_value(DistributionMode::Disabled).unwrap(),
            "disabled",
        ),
        (
            serde_json::to_value(DistributionMode::Enabled).unwrap(),
            "enabled",
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
            serde_json::to_value(MarkdownDialect::CommonMark).unwrap(),
            "common_mark",
        ),
        (
            serde_json::to_value(RawHtmlPolicy::Disabled).unwrap(),
            "disabled",
        ),
        (
            serde_json::to_value(CodeRenderingMode::EscapedPlainText).unwrap(),
            "escaped_plain_text",
        ),
        (
            serde_json::to_value(MermaidRenderingMode::Placeholder).unwrap(),
            "placeholder",
        ),
    ] {
        assert_eq!(value, serde_json::json!(expected));
    }

    assert_eq!(
        serde_json::to_value(SubscriptionSettings::Disabled).unwrap(),
        serde_json::json!({ "state": "disabled" })
    );
    assert_eq!(
        serde_json::to_value(SubscriptionSettings::Enabled {
            privacy_policy_revision: PrivacyPolicyRevision::new("2026-08-29").unwrap(),
        })
        .unwrap(),
        serde_json::json!({
            "state": "enabled",
            "privacy_policy_revision": "2026-08-29",
        })
    );
    assert_eq!(
        serde_json::to_value(PublicationTipSettings::Unconfigured).unwrap(),
        serde_json::json!({ "state": "unconfigured" })
    );
    let range =
        TipAmountRange::new(TipAmount::new(100).unwrap(), TipAmount::new(1_000).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(PublicationTipSettings::Configured {
            default: DefaultPostTipPolicy::Enabled,
            range,
        })
        .unwrap(),
        serde_json::json!({
            "state": "configured",
            "default": "enabled",
            "range": { "minimum": 100, "maximum": 1000 },
        })
    );

    let codes = [
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
            ContentValidationCode::SubscriptionPrivacyRevisionRequired,
            "subscription_privacy_revision_required",
        ),
        (
            ContentValidationCode::DistributionEnabledRequired,
            "distribution_enabled_required",
        ),
        (
            ContentValidationCode::TipRangeRequired,
            "tip_range_required",
        ),
        (
            ContentValidationCode::TipAmountInvalid,
            "tip_amount_invalid",
        ),
        (ContentValidationCode::TipRangeInvalid, "tip_range_invalid"),
        (
            ContentValidationCode::PostTipsUnconfigured,
            "post_tips_unconfigured",
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
