//! Published content, current Owner authority and real recipient admission meet
//! at the concrete SES boundary. The peer holds replies to order concurrent work.

use maincopy_shared::auth::{
    AdminSessionId, HumanLoginProvider, LoginChallengeId, UserRole, UserStatus,
};
use markdown_compiler::{PostCollection, PostId, prepare_content};

use super::*;
use crate::{
    admin::test_support::AdminTestRuntime,
    content_fixtures::{content_tree, post, publication},
    domain::{
        auth::{
            CsrfTokenDigest, LoginChallengeDigest, Nip98EventId, SessionTokenDigest,
            store::{
                AdminMutationKey, AuditPrincipalReference, CreateBrowserSession,
                CreateLoginChallenge, CreateUser, MutationAuditContext, ReplaceUserRoles,
                SessionAuditContext, SessionAuthenticationEvidence,
            },
        },
        mail::{
            announcement::Announcement,
            campaign::{Campaign, CampaignContent, CampaignCounts, CampaignId, CampaignState},
            control::{ControlClaims, ControlPurpose},
            store::{ApproveCampaign, CancelCampaign, CreateCampaign},
            subscriber::{
                ConfirmEnrollment, ControlOutcome, FinishAttempt, ManageEnrollment,
                RecipientHandle, SubmissionOutcome,
            },
        },
        publication::activation::PublishNow,
    },
    render::{compile_content_catalog, render_bound_post_preview},
};

const POST_ID: &str = "11111111-1111-4111-8111-111111111111";
const SECOND_ACCEPTED: &str = r#"{"MessageId":"010001-second-campaign-recipient"}"#;

struct CampaignFixture {
    mail: Fixture,
    runtime: AdminTestRuntime,
    session: AdminSessionId,
}

impl CampaignFixture {
    async fn start() -> Self {
        let mail = Fixture::start(10).await;
        let session = browser_session(&mail.store, mail.owner, 20).await;
        let runtime = AdminTestRuntime::start(&mail.store).await;
        let fixture = Self {
            mail,
            runtime,
            session,
        };
        fixture.publish("Reviewed public article").await;
        fixture
    }

    fn audit(&self) -> MutationAuditContext {
        audit(self.mail.owner, self.session)
    }

    async fn publish(&self, title: &str) {
        let tree = content_tree(
            publication("publication.toml", "[site]\ntitle = \"Dispatcher test\"\nbase_url = \"https://example.com/\"\ndescription = \"Public announcement fixture.\"\n[author]\nname = \"Test author\"\n".into()),
            vec![post("posts/article.md", PostCollection::Posts, format!("+++\nid = \"{POST_ID}\"\ntitle = \"{title}\"\nslug = \"article\"\nauthored_at = 2026-09-01T00:00:00Z\ndescription = \"Public description & summary.\"\ndraft = false\n+++\n\nA public article.\n"))],
            vec![], 0,
        );
        let handle = &self.runtime.state.publications;
        let catalog = Arc::new(compile_content_catalog(&prepare_content(&tree).unwrap()).unwrap());
        handle
            .apply_content_catalog(catalog, tree.digest(), None)
            .await
            .unwrap();
        let projection = handle.read();
        let post_id = PostId::parse(POST_ID).unwrap();
        let preview = render_bound_post_preview(
            &projection.catalog,
            projection.frontend,
            &post_id,
            projection.tip_recipient.as_ref(),
            &format!("/api/admin/v1/preview-assets/{}", projection.content_digest),
            projection
                .ledger
                .published_post(&post_id)
                .map(|entry| entry.published_at),
        )
        .unwrap()
        .unwrap();
        handle
            .publish_now(PublishNow {
                creation_key: Uuid::new_v4(),
                publication_id: Uuid::new_v4(),
                stable_post_id: post_id,
                expected_revision: Some(preview.revision),
                accepted_preview_digest: preview.digest,
            })
            .await
            .unwrap();
    }

    async fn approve(&self) -> Campaign {
        let projection = self.runtime.state.publications.read();
        let post_id = PostId::parse(POST_ID).unwrap();
        let revision = &projection.ledger.published_post(&post_id).unwrap().revision;
        let announcement = Announcement::from_published(
            &projection,
            &self.runtime.mail.snapshots.load_full(),
            &post_id,
            revision,
        )
        .unwrap();
        let content =
            CampaignContent::from_announcement(announcement, projection.site.version).unwrap();
        let draft = self
            .mail
            .store
            .mail
            .create_draft(CreateCampaign {
                proposed_id: CampaignId(Uuid::new_v4()),
                content,
                configuration_binding: self.mail.binding,
                audit: self.audit(),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();
        self.mail
            .store
            .mail
            .approve(ApproveCampaign {
                campaign_id: draft.campaign_id,
                expected_version: draft.version,
                configuration_binding: self.mail.binding,
                audit: self.audit(),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap()
    }

    async fn recipient(&self, index: u128, mailbox: &str) -> RecipientHandle {
        let recipient = RecipientHandle {
            enrollment: Uuid::parse_str(&format!("00000000-0000-4000-8000-{index:012x}")).unwrap(),
            generation: Uuid::new_v4(),
        };
        let address = EmailAddress::parse(mailbox).unwrap();
        let confirmation_attempt = Uuid::new_v4();
        assert_eq!(
            self.mail
                .store
                .subscribers
                .request_enrollment(RequestEnrollment {
                    mailbox_digest: SubscriberDigest::from_bytes(
                        self.mail.controls.mailbox_digest(&address)
                    ),
                    address,
                    enrollment: recipient.enrollment,
                    generation: recipient.generation,
                    confirmation_attempt,
                    configuration_binding: self.mail.binding,
                })
                .await
                .unwrap(),
            EnrollmentRequestResult::Queued
        );
        let expires_at =
            OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp() + 3600)
                .unwrap();
        let DeliveryAdmission::Ready(permit) = self
            .mail
            .store
            .subscribers
            .claim_confirmation(ClaimConfirmation {
                attempt_id: confirmation_attempt,
                nonce_digest: SubscriberDigest::from_bytes([8; 32]),
                expires_at,
                configuration_binding: self.mail.binding,
            })
            .await
            .unwrap()
        else {
            panic!("pending confirmation must be admitted")
        };
        let attempt = permit.into_attempt();
        self.mail
            .store
            .subscribers
            .finish_attempt(FinishAttempt {
                mail_epoch: attempt.mail_epoch,
                attempt_id: attempt.attempt_id,
                attempt_fence: attempt.attempt_fence,
                outcome: SubmissionOutcome::Accepted(
                    MessageId::parse(&format!("confirmation-{index}")).unwrap(),
                ),
            })
            .await
            .unwrap();
        assert_eq!(
            self.mail
                .store
                .subscribers
                .confirm(ConfirmEnrollment {
                    enrollment: recipient.enrollment,
                    generation: recipient.generation,
                    nonce_digest: SubscriberDigest::from_bytes([8; 32]),
                    expires_at,
                })
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        recipient
    }

    async fn current(&self, id: CampaignId) -> Campaign {
        self.mail.store.mail.campaign(id).await.unwrap().unwrap()
    }

    async fn wait_for(&self, id: CampaignId, state: &str) -> Campaign {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let current = self.current(id).await;
                if current.state.as_str() == state {
                    return current;
                }
                // The committed state is the handoff; elapsed time is never
                // evidence that the dispatcher has finished a transition.
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    async fn cancel(&self, id: CampaignId) -> Campaign {
        let current = self.current(id).await;
        self.mail
            .store
            .mail
            .cancel(CancelCampaign {
                campaign_id: id,
                expected_version: current.version,
                audit: self.audit(),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap()
    }

    async fn finish(mut self) {
        self.runtime.stop_actor().await;
        drop(self.runtime);
        self.mail.finish().await;
    }
}

fn audit(owner: UserId, session: AdminSessionId) -> MutationAuditContext {
    MutationAuditContext {
        audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
        principal: AuditPrincipalReference::BrowserSession {
            user_id: owner,
            session_id: session,
        },
        request_id: None,
        idempotency_key: AdminMutationKey(Uuid::new_v4()),
    }
}

async fn browser_session(store: &DatabaseStore, owner: UserId, seed: u8) -> AdminSessionId {
    let now = OffsetDateTime::now_utc();
    let challenge_id = LoginChallengeId::from_uuid(Uuid::new_v4());
    let challenge_digest = LoginChallengeDigest::parse_bytes(&[seed; 32]).unwrap();
    store
        .auth
        .create_login_challenge(CreateLoginChallenge {
            challenge_id,
            provider: HumanLoginProvider::Nostr,
            challenge_digest,
            created_at: now - time::Duration::seconds(10),
            expires_at: now + time::Duration::minutes(1),
        })
        .await
        .unwrap();
    let session_id = AdminSessionId::from_uuid(Uuid::new_v4());
    store
        .auth
        .create_browser_session(CreateBrowserSession {
            session_id,
            user_id: owner,
            expected_user_version: 1,
            session_token_digest: SessionTokenDigest::parse_bytes(&[seed + 1; 32]).unwrap(),
            csrf_token_digest: CsrfTokenDigest::parse_bytes(&[seed + 2; 32]).unwrap(),
            evidence: SessionAuthenticationEvidence::Nostr {
                expected_credential_version: 1,
                challenge_id,
                challenge_digest,
                event_id: Nip98EventId::parse_bytes(&[seed + 3; 32]).unwrap(),
                proof_created_at: now,
            },
            authenticated_at: now,
            fresh_until: now + time::Duration::hours(1),
            expires_at: now + time::Duration::hours(2),
            audit: SessionAuditContext {
                audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                request_id: None,
            },
        })
        .await
        .unwrap();
    session_id
}

fn assert_newsletter(
    fixture: &CampaignFixture,
    request: &CapturedRequest,
    mailbox: &str,
    recipient: &RecipientHandle,
    campaign: &Campaign,
) -> Uuid {
    assert_eq!(
        request.body["Destination"]["ToAddresses"],
        serde_json::json!([mailbox])
    );
    assert!(request.body["Destination"].get("CcAddresses").is_none());
    assert!(request.body["Destination"].get("BccAddresses").is_none());
    assert_eq!(request.body["ConfigurationSetName"], "maincopy-newsletter");
    assert_eq!(
        request.body["Content"]["Simple"]["Subject"]["Data"],
        campaign.content.subject
    );
    assert_eq!(request.body["EmailTags"].as_array().unwrap().len(), 3);
    assert_eq!(request.body["EmailTags"][0]["Name"], "maincopy-campaign");
    assert_eq!(
        request.body["EmailTags"][0]["Value"],
        campaign.campaign_id.0.to_string()
    );
    assert_eq!(request.body["EmailTags"][1]["Name"], "maincopy-attempt");
    assert_eq!(request.body["EmailTags"][2]["Name"], "maincopy-epoch");
    let attempt = Uuid::parse_str(request.body["EmailTags"][1]["Value"].as_str().unwrap()).unwrap();
    let headers = request.body["Content"]["Simple"]["Headers"]
        .as_array()
        .unwrap();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0]["Name"], "List-Unsubscribe");
    assert_eq!(
        headers[1],
        serde_json::json!({"Name": "List-Unsubscribe-Post", "Value": "List-Unsubscribe=One-Click"})
    );
    let removal = headers[0]["Value"]
        .as_str()
        .unwrap()
        .strip_prefix('<')
        .unwrap()
        .strip_suffix('>')
        .unwrap();
    let token = removal
        .strip_prefix("https://example.com/email/unsubscribe/")
        .unwrap();
    assert!(
        fixture
            .mail
            .controls
            .verify(ControlPurpose::Manage, token, OffsetDateTime::now_utc())
            .unwrap()
            == ControlClaims::Manage {
                enrollment: recipient.enrollment,
                generation: recipient.generation,
            }
    );
    let text = request.body["Content"]["Simple"]["Body"]["Text"]["Data"]
        .as_str()
        .unwrap();
    assert!(text.starts_with(&campaign.content.text));
    assert!(text.contains(removal));
    assert!(text.contains("123 Example Street"));
    let html = request.body["Content"]["Simple"]["Body"]["Html"]["Data"]
        .as_str()
        .unwrap();
    assert!(html.starts_with(&campaign.content.html));
    assert!(html.contains("Unsubscribe and remove my address"));
    attempt
}

#[tokio::test]
async fn approved_publication_sends_serially_to_the_frozen_audience_then_completes() {
    let mut fixture = CampaignFixture::start().await;
    let first = fixture.recipient(1, "First@example.com").await;
    let second = fixture.recipient(2, "second@example.com").await;
    let campaign = fixture.approve().await;
    assert!(
        matches!(campaign.state, CampaignState::Queued { ref approval } if approval.audience_cutoff == 2)
    );
    fixture.recipient(3, "after-approval@example.com").await;
    let task = fixture.mail.dispatch();
    let request = fixture.mail.request().await;
    let first_attempt =
        assert_newsletter(&fixture, &request, "First@example.com", &first, &campaign);
    assert_eq!(fixture.mail.outcome(first_attempt).await, "admitted");
    fixture.publish("Later public revision").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(250), fixture.mail.requests.recv())
            .await
            .is_err(),
        "one recipient request must finish before another starts"
    );
    request.reply.send(ACCEPTED).unwrap();
    let request = fixture.mail.request().await;
    let second_attempt =
        assert_newsletter(&fixture, &request, "second@example.com", &second, &campaign);
    assert_ne!(first_attempt, second_attempt);
    assert_eq!(fixture.mail.outcome(first_attempt).await, "accepted");
    request.reply.send(SECOND_ACCEPTED).unwrap();
    let completed = fixture.wait_for(campaign.campaign_id, "completed").await;
    assert!(
        matches!(completed.state, CampaignState::Completed { counts, .. } if counts == (CampaignCounts { accepted: 2, rejected: 0, unknown: 0 }))
    );
    task.finish().await;
    assert_eq!(fixture.mail.outcome(second_attempt).await, "accepted");
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mail_attempts WHERE campaign_id=?")
            .bind(campaign.campaign_id.0.as_bytes().as_slice())
            .fetch_one(&mut fixture.mail.reader)
            .await
            .unwrap();
    assert_eq!(attempts, 2);
    let budget: (i64, i64) =
        sqlx::query_as("SELECT SUM(total),SUM(confirmations) FROM mail_daily_budget")
            .fetch_one(&mut fixture.mail.reader)
            .await
            .unwrap();
    assert_eq!(budget, (5, 3));
    let restarted = fixture.mail.dispatch();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), fixture.mail.requests.recv())
            .await
            .is_err()
    );
    restarted.finish().await;
    assert_eq!(fixture.current(campaign.campaign_id).await, completed);
    fixture.finish().await;
}

#[tokio::test]
async fn owner_cancellation_drains_the_inflight_recipient_and_finishes_cancelled() {
    let mut fixture = CampaignFixture::start().await;
    let first = fixture.recipient(1, "first@example.com").await;
    fixture.recipient(2, "must-not-send@example.com").await;
    let campaign = fixture.approve().await;
    let task = fixture.mail.dispatch();
    let request = fixture.mail.request().await;
    let attempt = assert_newsletter(&fixture, &request, "first@example.com", &first, &campaign);
    assert!(matches!(
        fixture.cancel(campaign.campaign_id).await.state,
        CampaignState::Cancelling { .. }
    ));
    assert_eq!(fixture.mail.outcome(attempt).await, "admitted");
    assert!(!task.task.is_finished());
    request.reply.send(ACCEPTED).unwrap();
    let cancelled = fixture.wait_for(campaign.campaign_id, "cancelled").await;
    assert!(
        matches!(cancelled.state, CampaignState::Cancelled { counts, .. } if counts == (CampaignCounts { accepted: 1, rejected: 0, unknown: 0 }))
    );
    task.finish().await;
    assert_eq!(fixture.mail.outcome(attempt).await, "accepted");
    assert!(fixture.mail.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn removal_before_admission_excludes_a_previously_approved_recipient() {
    let mut fixture = CampaignFixture::start().await;
    fixture.recipient(1, "first@example.com").await;
    let removed = fixture.recipient(2, "removed@example.com").await;
    let campaign = fixture.approve().await;
    let task = fixture.mail.dispatch();
    let request = fixture.mail.request().await;
    assert_eq!(
        request.body["Destination"]["ToAddresses"],
        serde_json::json!(["first@example.com"])
    );
    assert_eq!(
        fixture
            .mail
            .store
            .subscribers
            .remove(ManageEnrollment {
                enrollment: removed.enrollment,
                generation: removed.generation,
            })
            .await
            .unwrap(),
        ControlOutcome::Changed
    );
    request.reply.send(ACCEPTED).unwrap();
    let completed = fixture.wait_for(campaign.campaign_id, "completed").await;
    assert!(
        matches!(completed.state, CampaignState::Completed { counts, .. } if counts.accepted == 1)
    );
    task.finish().await;
    let retained: (i64, i64) = sqlx::query_as(
        "SELECT count(address),count(mailbox_digest) FROM mail_enrollments WHERE enrollment_id=?",
    )
    .bind(removed.enrollment.as_bytes().as_slice())
    .fetch_one(&mut fixture.mail.reader)
    .await
    .unwrap();
    assert_eq!(retained, (0, 0));
    assert!(fixture.mail.requests.try_recv().is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn daily_budget_defers_without_spending_or_finishing_the_campaign() {
    let mut fixture = CampaignFixture::start().await;
    fixture.recipient(1, "first@example.com").await;
    let campaign = fixture.approve().await;
    let policy = SubscriberPolicy {
        configuration_binding: fixture.mail.binding,
        mode: SubscriberMode::Enabled,
        max_daily_messages: 1,
        max_daily_confirmations: 1,
        max_campaign_recipients: 10,
    };
    fixture
        .mail
        .store
        .subscribers
        .set_policy(policy)
        .await
        .unwrap();
    ready_feedback(&fixture.mail.store, fixture.mail.binding).await;
    let task = fixture.mail.dispatch();
    fixture.wait_for(campaign.campaign_id, "claimed").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(350), fixture.mail.requests.recv())
            .await
            .is_err()
    );
    assert!(matches!(
        fixture.current(campaign.campaign_id).await.state,
        CampaignState::Claimed { .. }
    ));
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mail_attempts WHERE campaign_id=?")
            .bind(campaign.campaign_id.0.as_bytes().as_slice())
            .fetch_one(&mut fixture.mail.reader)
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    fixture
        .mail
        .store
        .subscribers
        .set_policy(SubscriberPolicy {
            max_daily_messages: 2,
            ..policy
        })
        .await
        .unwrap();
    ready_feedback(&fixture.mail.store, fixture.mail.binding).await;
    fixture.mail.request().await.reply.send(ACCEPTED).unwrap();
    let completed = fixture.wait_for(campaign.campaign_id, "completed").await;
    assert!(
        matches!(completed.state, CampaignState::Completed { counts, .. } if counts.accepted == 1)
    );
    task.finish().await;
    let total: i64 = sqlx::query_scalar("SELECT SUM(total) FROM mail_daily_budget")
        .fetch_one(&mut fixture.mail.reader)
        .await
        .unwrap();
    assert_eq!(total, 2);
    fixture.finish().await;
}

#[tokio::test]
async fn revoking_the_approving_owners_role_stops_further_recipient_admission() {
    let mut fixture = CampaignFixture::start().await;
    fixture.recipient(1, "already-admitted@example.com").await;
    fixture.recipient(2, "must-not-send@example.com").await;
    let second_owner = UserId::from_uuid(Uuid::new_v4());
    let key = SigningKey::from_bytes(&[9; 32]).unwrap();
    fixture
        .mail
        .store
        .auth
        .create_user(CreateUser {
            user_id: second_owner,
            created_by_user_id: fixture.mail.owner,
            status: UserStatus::Enabled,
            roles: [UserRole::Owner].into_iter().collect(),
            credentials: vec![NewHumanCredential::Nostr {
                public_key: NostrPublicKey::from_bytes(key.verifying_key().to_bytes().into())
                    .unwrap(),
            }],
            configured_providers: ConfiguredLoginProviders::new(false, true).unwrap(),
            occurred_at: OffsetDateTime::now_utc(),
            audit: fixture.audit(),
        })
        .await
        .unwrap();
    let second_session = browser_session(&fixture.mail.store, second_owner, 40).await;
    let campaign = fixture.approve().await;
    let task = fixture.mail.dispatch();
    let request = fixture.mail.request().await;
    let attempt = Uuid::parse_str(request.body["EmailTags"][1]["Value"].as_str().unwrap()).unwrap();
    fixture
        .mail
        .store
        .auth
        .replace_user_roles(ReplaceUserRoles {
            user_id: fixture.mail.owner,
            expected_version: 1,
            roles: [UserRole::Administrator].into_iter().collect(),
            assigned_by_user_id: second_owner,
            occurred_at: OffsetDateTime::now_utc(),
            audit: audit(second_owner, second_session),
        })
        .await
        .unwrap();
    request.reply.send(ACCEPTED).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(350), fixture.mail.requests.recv())
            .await
            .is_err()
    );
    task.finish().await;
    assert_eq!(fixture.mail.outcome(attempt).await, "accepted");
    assert!(matches!(
        fixture.current(campaign.campaign_id).await.state,
        CampaignState::Claimed { .. }
    ));
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mail_attempts WHERE campaign_id=?")
            .bind(campaign.campaign_id.0.as_bytes().as_slice())
            .fetch_one(&mut fixture.mail.reader)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    fixture.finish().await;
}
