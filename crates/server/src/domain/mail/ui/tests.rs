use super::*;
use axum::{
    body::Body,
    extract::FromRequest as _,
    http::{Method, Request, header::CONTENT_TYPE},
};
use maincopy_shared::auth::UserId;
use serde::de::DeserializeOwned;

use crate::domain::mail::{
    announcement::announcement_content_digest,
    campaign::{CampaignApproval, CampaignFence, CampaignLease},
    config::{MailConfiguration, MailConfigurationCandidate},
    subscriber::SubscriberPolicy,
};

fn binding() -> MailReviewBinding {
    let candidate: MailConfigurationCandidate = toml::from_str(
        r#"
mode = "ses"
sender = "Newsletter@EXAMPLE.COM"
region = "us-east-1"
configuration_set = "newsletter"
credential_file = "secrets/credentials.json"
control_signing_key_file = "secrets/control.key"
"#,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let MailConfiguration::Ses(configuration) = candidate.validate(root.path()).unwrap() else {
        panic!("fixture selects SES")
    };
    let credentials = SesCredentials::parse(
        br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#,
    )
    .unwrap();
    MailReviewBinding::from_configuration(*configuration, &credentials)
}

fn content() -> CampaignContent {
    let mut value = CampaignContent {
        post_id: PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        revision: PostRevisionDigest::from_bytes([1; 32]),
        snapshot: SiteSnapshotDigest::from_bytes([2; 32]),
        site_version: 1,
        template_version: 1,
        canonical_url: "https://example.com/posts/article".into(),
        subject: "A <reviewed> article".into(),
        text: "A <script>description</script> & summary".into(),
        html: "<p onclick=\"alert(1)\">Saved <script>hostile()</script> HTML</p>".into(),
        content_digest: [0; 32],
    };
    value.content_digest = announcement_content_digest(
        &value.post_id,
        &value.revision,
        &value.canonical_url,
        &value.subject,
        &value.text,
        &value.html,
    );
    value.validate().unwrap();
    value
}

fn campaign(binding: &MailReviewBinding) -> Campaign {
    Campaign {
        campaign_id: CampaignId(Uuid::new_v4()),
        version: CampaignVersion::INITIAL,
        content: content(),
        configuration_binding: binding.configuration_binding,
        created_by: UserId::from_uuid(Uuid::new_v4()),
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
        state: CampaignState::Draft,
    }
}

fn approval() -> CampaignApproval {
    CampaignApproval {
        owner: UserId::from_uuid(Uuid::new_v4()),
        approved_at: OffsetDateTime::UNIX_EPOCH,
        instance_version: 1,
        audience_cutoff: 0,
    }
}

fn lease() -> CampaignLease {
    CampaignLease {
        fence: CampaignFence(Uuid::new_v4()),
        claimed_at: OffsetDateTime::UNIX_EPOCH,
        renewed_at: OffsetDateTime::UNIX_EPOCH,
        expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(30),
    }
}

fn review_form(content: &CampaignContent, binding: &[u8; 32]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("_csrf", "csrf-fixture")
        .append_pair("idempotency_key", &Uuid::new_v4().to_string())
        .append_pair("proposed_id", &Uuid::new_v4().to_string())
        .append_pair("revision", content.revision.as_str())
        .append_pair("snapshot", content.snapshot.as_str())
        .append_pair("site_version", &content.site_version.to_string())
        .append_pair("content_digest", &hex(&content.content_digest))
        .append_pair("configuration_binding", &hex(binding))
        .finish()
}

#[test]
fn stored_announcement_and_provider_metadata_are_safe_to_inspect() {
    let preview = content_preview(&content()).into_string();
    assert!(preview.contains("&lt;reviewed&gt;"));
    assert!(preview.contains("&lt;script&gt;description&lt;/script&gt;"));
    assert!(preview.contains("&lt;p onclick=&quot;alert(1)&quot;&gt;"));
    assert!(!preview.contains("<script>"));
    assert!(!preview.contains("<p onclick="));
    assert!(preview.contains("href=\"https://example.com/posts/article\""));
    let provider = provider_panel(&binding()).into_string();
    assert!(provider.contains("Newsletter@example.com"));
    assert!(provider.contains("2000"));
    assert!(!provider.contains("credentials.json"));
    assert!(!provider.contains("control.key"));
    assert!(!provider.contains("AKIDEXAMPLE"));
}

#[tokio::test]
async fn draft_form_binds_each_review_dimension_and_preserves_operation_identity() {
    let content = content();
    let binding = binding();
    let encoded = review_form(&content, &binding.configuration_binding);
    let original: ReviewForm = parse_form(&encoded).await.unwrap();
    original
        .matches(&content.post_id, &content, &binding.configuration_binding)
        .unwrap();
    for field in [
        "post", "revision", "snapshot", "site", "content", "provider",
    ] {
        let mut changed = content.clone();
        let mut provider = binding.configuration_binding;
        match field {
            "post" => {
                changed.post_id = PostId::parse("22222222-2222-4222-8222-222222222222").unwrap()
            }
            "revision" => changed.revision = PostRevisionDigest::from_bytes([8; 32]),
            "snapshot" => changed.snapshot = SiteSnapshotDigest::from_bytes([8; 32]),
            "site" => changed.site_version += 1,
            "content" => changed.content_digest = [8; 32],
            "provider" => provider = [8; 32],
            _ => unreachable!(),
        }
        assert!(
            matches!(
                original.matches(&content.post_id, &changed, &provider),
                Err(UiError::ReviewChanged)
            ),
            "{field}"
        );
    }
    let retry: ReviewForm = parse_form(&encoded).await.unwrap();
    assert_eq!(
        canonical_uuid(&original.idempotency_key).unwrap(),
        canonical_uuid(&retry.idempotency_key).unwrap()
    );
    assert_eq!(
        canonical_uuid(&original.proposed_id).unwrap(),
        canonical_uuid(&retry.proposed_id).unwrap()
    );
}

#[tokio::test]
async fn native_forms_reject_ambiguous_versions_digests_and_ignored_fields() {
    let content = content();
    let provider = binding();
    let encoded = review_form(&content, &provider.configuration_binding);
    for suffix in [
        "&_csrf=second",
        "&idempotency_key=second",
        "&site_version=2",
        "&ignored=1",
    ] {
        assert!(
            parse_form::<ReviewForm>(&format!("{encoded}{suffix}"))
                .await
                .is_err()
        );
    }
    for invalid in ["0", "9223372036854775808"] {
        assert!(
            parse_form::<ReviewForm>(
                &encoded.replace("site_version=1", &format!("site_version={invalid}"))
            )
            .await
            .is_err()
        );
    }
    let mut form: ReviewForm = parse_form(&encoded).await.unwrap();
    form.content_digest = "f".repeat(63).into_boxed_str();
    assert!(
        form.matches(&content.post_id, &content, &provider.configuration_binding)
            .is_err()
    );
    form.content_digest = hex(&content.content_digest).to_uppercase().into_boxed_str();
    assert!(
        form.matches(&content.post_id, &content, &provider.configuration_binding)
            .is_err()
    );
    let uuid = Uuid::new_v4();
    assert!(canonical_uuid(&uuid.simple().to_string()).is_err());
    assert!(canonical_uuid(&uuid.to_string().to_uppercase()).is_err());
    assert!(canonical_uuid("malformed").is_err());
}

#[test]
fn only_ready_unchanged_provider_configuration_can_approve_a_draft() {
    let binding = binding();
    let draft = campaign(&binding);
    assert!(matches!(
        MailUiAccess::Unavailable.approval_binding(&draft, &healthy_readiness(&binding)),
        Err(UiError::DispatchUnavailable)
    ));
    assert!(matches!(
        MailUiAccess::ReviewOnly(binding.clone())
            .approval_binding(&draft, &healthy_readiness(&binding)),
        Err(UiError::DispatchUnavailable)
    ));
    let ready = MailUiAccess::DispatchReady(binding.clone());
    assert_eq!(
        ready
            .approval_binding(&draft, &healthy_readiness(&binding))
            .unwrap(),
        draft.configuration_binding
    );
    let mut changed = draft.clone();
    changed.configuration_binding = [9; 32];
    assert!(matches!(
        ready.approval_binding(&changed, &healthy_readiness(&binding)),
        Err(UiError::ConfigurationChanged)
    ));
    let controls =
        campaign_controls(&draft, &ready, &healthy_readiness(&binding), "csrf", true).into_string();
    assert!(controls.contains("Approve sending"));
    assert!(controls.contains("name=\"expected_version\" value=\"1\""));
    assert!(controls.contains("name=\"configuration_binding\""));
    assert!(
        !campaign_controls(&changed, &ready, &healthy_readiness(&binding), "csrf", true)
            .into_string()
            .contains("Approve sending")
    );
}

#[test]
fn unavailable_mail_still_allows_cancellation_and_exact_approval_receipt_recovery() {
    let binding = binding();
    let mut campaign = campaign(&binding);
    let unavailable = MailUiAccess::Unavailable;
    let controls = campaign_controls(
        &campaign,
        &unavailable,
        &SubscriberReadiness::Unavailable,
        "csrf",
        true,
    )
    .into_string();
    assert!(controls.contains("Cancel campaign"));
    assert!(!controls.contains("Approve sending"));
    campaign.state = CampaignState::Queued {
        approval: approval(),
    };
    assert_eq!(
        unavailable
            .approval_binding(&campaign, &SubscriberReadiness::Unavailable)
            .unwrap(),
        campaign.configuration_binding
    );
    assert!(
        !campaign_controls(
            &campaign,
            &unavailable,
            &SubscriberReadiness::Unavailable,
            "csrf",
            true
        )
        .into_string()
        .contains("Approve sending")
    );
    campaign.state = CampaignState::Cancelled {
        approval: None,
        counts: CampaignCounts::default(),
    };
    assert_eq!(
        unavailable
            .approval_binding(&campaign, &SubscriberReadiness::Unavailable)
            .unwrap(),
        campaign.configuration_binding
    );
    assert!(
        !campaign_controls(
            &campaign,
            &unavailable,
            &SubscriberReadiness::Unavailable,
            "csrf",
            true
        )
        .into_string()
        .contains("Cancel campaign")
    );
}

#[test]
fn stale_browser_session_can_review_but_mutation_controls_require_reauthentication() {
    let binding = binding();
    let campaign = campaign(&binding);
    let controls = campaign_controls(
        &campaign,
        &MailUiAccess::DispatchReady(binding.clone()),
        &healthy_readiness(&binding),
        "csrf",
        false,
    )
    .into_string();
    assert_eq!(controls.matches("disabled").count(), 2);
    assert!(controls.contains("Sign out and sign in again"));
    let draft = draft_form(
        &campaign.content,
        &binding.configuration_binding,
        "csrf",
        false,
    )
    .into_string();
    assert!(draft.contains("disabled"));
    assert!(draft.contains("name=\"proposed_id\""));
    assert!(
        !content_preview(&campaign.content)
            .into_string()
            .contains("disabled")
    );
}

#[test]
fn unknown_and_unreconciled_outcomes_do_not_imply_zero_or_successful_delivery() {
    let counts = CampaignCounts {
        accepted: 7,
        rejected: 2,
        unknown: 3,
    };
    let unknown = state_panel(&CampaignState::Unknown {
        approval: approval(),
        counts,
    })
    .into_string();
    assert!(unknown.contains("Do not resend automatically"));
    assert!(unknown.contains("<dd>7</dd>"));
    assert!(unknown.contains("<dd>3</dd>"));
    for reason in [
        CampaignQuarantine::Interrupted,
        CampaignQuarantine::Restore {
            restore_id: Uuid::new_v4(),
        },
    ] {
        let markup = state_panel(&CampaignState::Quarantined {
            approval: Some(approval()),
            progress: CampaignProgress::Unreconciled,
            reason,
        })
        .into_string();
        assert!(markup.contains("Submission counts are unreconciled and unavailable"));
        assert!(!markup.contains("<dd>0</dd>"));
        assert!(!markup.contains("Provider accepted"));
    }
    let completed = state_panel(&CampaignState::Completed {
        approval: approval(),
        counts: CampaignCounts {
            accepted: 7,
            rejected: 2,
            unknown: 0,
        },
    })
    .into_string();
    assert!(completed.contains("not that a message reached an inbox"));
    let known = state_panel(&CampaignState::Quarantined {
        approval: Some(approval()),
        progress: CampaignProgress::Known(counts),
        reason: CampaignQuarantine::Interrupted,
    })
    .into_string();
    assert!(known.contains("<dd>7</dd>"));
}

#[test]
fn active_campaign_states_explain_cancellation_and_do_not_invent_counts() {
    for state in [
        CampaignState::Draft,
        CampaignState::Queued {
            approval: approval(),
        },
        CampaignState::Claimed {
            approval: approval(),
            lease: lease(),
        },
        CampaignState::Cancelling {
            approval: approval(),
            lease: lease(),
        },
    ] {
        let markup = state_panel(&state).into_string();
        assert!(!markup.contains("Provider accepted"));
        assert!(!markup.contains("<dd>0</dd>"));
    }
    let cancelling = state_panel(&CampaignState::Cancelling {
        approval: approval(),
        lease: lease(),
    })
    .into_string();
    assert!(cancelling.contains("Messages already being submitted may still be sent"));
}

#[test]
fn published_article_selection_has_a_bounded_continuation() {
    let ids = (1..=PAGE_SIZE + 1)
        .map(|index| PostId::parse(&Uuid::from_u128(index as u128).to_string()).unwrap())
        .collect::<Vec<_>>();
    let posts = ids
        .iter()
        .map(|post_id| PublishedChoice {
            post_id,
            title: "A <public> title",
        })
        .collect::<Vec<_>>();
    let markup = published_choices(&posts).into_string();
    assert!(markup.contains("A &lt;public&gt; title"));
    assert_eq!(markup.matches("/review\"").count(), PAGE_SIZE);
    assert!(markup.contains(&format!("post_after={}", posts[PAGE_SIZE - 1].post_id)));
    assert!(!markup.contains(posts[PAGE_SIZE].post_id.as_str()));
}

#[test]
fn unknown_mutation_and_receipt_conflicts_give_distinct_recovery_instructions() {
    let (status, unknown) = command_error(CampaignCommandError::OutcomeUnknown);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(unknown.contains("same session"));
    assert!(unknown.contains("original operation ID"));
    assert!(unknown.contains("Do not create another campaign"));
    let (status, conflict) = command_error(CampaignCommandError::IdempotencyConflict);
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(conflict.contains("different form or browser session"));
}

#[path = "router_tests.rs"]
mod router_tests;

async fn parse_form<T: DeserializeOwned>(body: &str) -> Result<T, FormRejection> {
    let request = Request::builder()
        .method(Method::POST)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    Form::<T>::from_request(request, &())
        .await
        .map(|Form(value)| value)
}

fn healthy_status(binding: &MailReviewBinding) -> SubscriberStatus {
    let config = binding.configuration.view();
    SubscriberStatus {
        control_version: 1,
        mail_epoch: Some(Uuid::from_u128(1)),
        policy: Some(SubscriberPolicy {
            configuration_binding: binding.configuration_binding,
            mode: SubscriberMode::Enabled,
            max_daily_messages: config.max_daily_messages,
            max_daily_confirmations: config.max_daily_confirmation_messages,
            max_campaign_recipients: config.max_campaign_recipients,
        }),
        feedback_health: FeedbackHealth::Healthy,
        last_feedback_ok_at: Some(OffsetDateTime::UNIX_EPOCH),
        active_enrollments: 7,
        pending_enrollments: 3,
        addressed_enrollments: 10,
        retained_enrollments: 12,
    }
}

fn healthy_readiness(binding: &MailReviewBinding) -> SubscriberReadiness {
    SubscriberReadiness::Observed(healthy_status(binding))
}

#[test]
fn sending_capability_never_overrides_live_pause_feedback_or_configuration() {
    let binding = binding();
    let campaign = campaign(&binding);
    let access = MailUiAccess::DispatchReady(binding.clone());
    let baseline = healthy_status(&binding);
    assert!(
        availability_panel(&access, &SubscriberReadiness::Observed(baseline))
            .into_string()
            .contains("Sending is ready.")
    );
    for reason in [
        "paused",
        "feedback",
        "reconciliation",
        "configuration",
        "unconfigured",
        "unavailable",
    ] {
        let mut status = baseline;
        let readiness = match reason {
            "paused" => {
                status.policy.as_mut().unwrap().mode = SubscriberMode::Paused;
                SubscriberReadiness::Observed(status)
            }
            "feedback" => {
                status.feedback_health = FeedbackHealth::Unavailable;
                SubscriberReadiness::Observed(status)
            }
            "reconciliation" => {
                status.feedback_health = FeedbackHealth::ReconciliationRequired;
                SubscriberReadiness::Observed(status)
            }
            "configuration" => {
                status.policy.as_mut().unwrap().configuration_binding = [5; 32];
                SubscriberReadiness::Observed(status)
            }
            "unconfigured" => {
                status.policy = None;
                SubscriberReadiness::Observed(status)
            }
            "unavailable" => SubscriberReadiness::Unavailable,
            _ => unreachable!(),
        };
        assert!(
            access.approval_binding(&campaign, &readiness).is_err(),
            "{reason}"
        );
        let availability = availability_panel(&access, &readiness).into_string();
        assert!(!availability.contains("Sending is ready."), "{reason}");
        let controls =
            campaign_controls(&campaign, &access, &readiness, "csrf", true).into_string();
        assert!(!controls.contains("Approve sending"), "{reason}");
        assert!(controls.contains("Cancel campaign"), "{reason}");
    }
}

#[test]
fn readiness_preserves_unknown_counts_and_explains_latched_feedback() {
    let unavailable = subscriber_panel(&SubscriberReadiness::Unavailable).into_string();
    assert!(unavailable.contains("counts are unavailable"));
    assert!(!unavailable.contains("<dd>0</dd>"));
    let binding = binding();
    let mut status = healthy_status(&binding);
    status.feedback_health = FeedbackHealth::ReconciliationRequired;
    let observed = subscriber_panel(&SubscriberReadiness::Observed(status)).into_string();
    assert!(observed.contains("Sending stays paused until the operator resolves"));
    assert!(observed.contains("<dt>Confirmed subscriptions</dt><dd>7</dd>"));
    assert!(observed.contains("<dt>Stored email addresses</dt><dd>10</dd>"));
    assert!(observed.contains("Last successful feedback check"));
    assert!(!observed.contains("Delivery feedback is current."));
}
