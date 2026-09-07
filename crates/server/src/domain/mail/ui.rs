//! Owner browser review of immutable public announcements and campaign outcomes.
//! Runtime composition supplies actual loaded provider identity; configuration
//! alone never authorizes dispatch. All mutations retain the writer's session-
//! bound receipt, current-owner, publication and version checks.

use axum::{
    Extension, Form, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, rejection::FormRejection},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use maincopy_shared::{
    auth::{UserRole, UserStatus},
    auth_api::SecretString,
};
use markdown_compiler::{PostId, PostRevisionDigest, SiteSnapshotDigest};
use maud::{Markup, html};
use serde::Deserialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::{
    announcement::{Announcement, AnnouncementError},
    campaign::{
        Campaign, CampaignContent, CampaignCounts, CampaignId, CampaignProgress,
        CampaignQuarantine, CampaignState, CampaignVersion,
    },
    config::SesMailConfiguration,
    ses::SesCredentials,
    store::{
        ApproveCampaign, CampaignCommandError, CampaignMutationError, CampaignPage, CampaignStore,
        CancelCampaign, CreateCampaign,
    },
    subscriber::{
        FeedbackHealth, SubscriberMode, SubscriberStatus,
        store::{SubscriberMutationError, SubscriberStore},
    },
};
use crate::{
    admin::{
        AdminRuntimeState, AdminSecurityState, BrowserFormSession, RequiredBrowserSession,
        browser_session_router,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{self as admin_ui, PageKind},
    },
    domain::{
        auth::store::AdminMutationKey,
        publication::activation::{PublicationCoordinatorHandle, PublicationReadProjection},
    },
    render::SiteSnapshotReader,
};

#[path = "ui/recovery.rs"]
mod recovery;

const PAGE_SIZE: usize = 20;
const MAX_FORM_BYTES: usize = 4096;

#[derive(Clone)]
pub(crate) struct MailUiState {
    pub campaigns: CampaignStore,
    pub subscribers: SubscriberStore,
    pub publications: PublicationCoordinatorHandle,
    pub snapshots: SiteSnapshotReader,
    pub access: MailUiAccess,
}

/// Runtime composition supplies the supervised sending capability. Current
/// subscriber policy and feedback readiness are checked separately for every
/// review and new approval. ReviewOnly permits drafts without sending.
#[derive(Clone)]
pub(crate) enum MailUiAccess {
    Unavailable,
    ReviewOnly(MailReviewBinding),
    DispatchReady(MailReviewBinding),
}

#[derive(Clone)]
pub(crate) struct MailReviewBinding {
    configuration: SesMailConfiguration,
    configuration_binding: [u8; 32],
}

impl MailReviewBinding {
    pub(super) fn from_configuration(
        configuration: SesMailConfiguration,
        credentials: &SesCredentials,
    ) -> Self {
        let configuration_binding = configuration.provider_binding(credentials);
        Self {
            configuration,
            configuration_binding,
        }
    }
}

impl MailUiAccess {
    fn review(&self) -> Result<&MailReviewBinding, UiError> {
        match self {
            Self::Unavailable => Err(UiError::ConfigurationUnavailable),
            Self::ReviewOnly(binding) | Self::DispatchReady(binding) => Ok(binding),
        }
    }

    fn approval_binding(
        &self,
        campaign: &Campaign,
        readiness: &SubscriberReadiness,
    ) -> Result<[u8; 32], UiError> {
        // A terminal or already-approved campaign can only recover an exact
        // existing receipt: the writer rejects every new non-Draft approval.
        if !matches!(campaign.state, CampaignState::Draft) {
            return Ok(campaign.configuration_binding);
        }
        match self {
            Self::DispatchReady(binding)
                if binding.configuration_binding == campaign.configuration_binding =>
            {
                readiness.require_ready(&binding.configuration_binding)?;
                Ok(binding.configuration_binding)
            }
            Self::DispatchReady(_) => Err(UiError::ConfigurationChanged),
            Self::Unavailable | Self::ReviewOnly(_) => Err(UiError::DispatchUnavailable),
        }
    }
}

/// A failed aggregate read is unavailable, never a known empty subscriber list.
enum SubscriberReadiness {
    Unavailable,
    Observed(SubscriberStatus),
}

impl SubscriberReadiness {
    async fn load(store: &SubscriberStore) -> Self {
        match store.status().await {
            Ok(status) => Self::Observed(status),
            Err(_) => Self::Unavailable,
        }
    }

    fn require_ready(&self, binding: &[u8; 32]) -> Result<(), UiError> {
        let Self::Observed(status) = self else {
            return Err(UiError::DispatchUnavailable);
        };
        let policy = status.policy.ok_or(UiError::DispatchUnavailable)?;
        if policy.configuration_binding != *binding {
            return Err(UiError::ConfigurationChanged);
        }
        if policy.mode != SubscriberMode::Enabled
            || status.feedback_health != FeedbackHealth::Healthy
        {
            return Err(UiError::DispatchUnavailable);
        }
        Ok(())
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    after: Option<Box<str>>,
    post_after: Option<PostId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
    proposed_id: Box<str>,
    revision: PostRevisionDigest,
    snapshot: SiteSnapshotDigest,
    site_version: CampaignVersion,
    content_digest: Box<str>,
    configuration_binding: Box<str>,
}

impl ReviewForm {
    fn matches(
        &self,
        post_id: &PostId,
        content: &CampaignContent,
        binding: &[u8; 32],
    ) -> Result<(), UiError> {
        if content.post_id != *post_id
            || content.revision != self.revision
            || content.snapshot != self.snapshot
            || content.site_version != u64::from(self.site_version)
            || hex(&content.content_digest) != self.content_digest.as_ref()
            || hex(binding) != self.configuration_binding.as_ref()
        {
            return Err(UiError::ReviewChanged);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
    expected_version: CampaignVersion,
    configuration_binding: Box<str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
    idempotency_key: Box<str>,
    expected_version: CampaignVersion,
}

pub(crate) fn router(
    security: &AdminSecurityState,
    state: MailUiState,
) -> Router<AdminRuntimeState> {
    browser_session_router(
        Router::new()
            .route("/admin/mail", get(history))
            .route(
                "/admin/mail/recovery",
                get(recovery::review).post(recovery::reset),
            )
            .route(
                "/admin/mail/posts/{post_id}/review",
                get(review).post(create),
            )
            .route("/admin/mail/campaigns/{campaign_id}", get(inspect))
            .route("/admin/mail/campaigns/{campaign_id}/approve", post(approve))
            .route("/admin/mail/campaigns/{campaign_id}/cancel", post(cancel))
            .route_layer(middleware::from_fn(require_owner))
            .layer(DefaultBodyLimit::max(MAX_FORM_BYTES))
            .layer(Extension(state)),
        security,
    )
    .layer(middleware::from_fn(admin_ui::adapt_security_response))
}

async fn require_owner(browser: RequiredBrowserSession, request: Request, next: Next) -> Response {
    match browser.security.store.user(browser.session.user_id).await {
        Ok(Some(user))
            if user.status == UserStatus::Enabled && user.roles.contains(&UserRole::Owner) =>
        {
            next.run(request).await
        }
        Ok(_) => admin_ui::error_response(
            StatusCode::FORBIDDEN,
            "Owner access required",
            "Only a currently enabled Owner can inspect or manage mail campaigns.",
            browser.request_id,
        ),
        Err(_) => UiError::Unavailable.response(browser.request_id),
    }
}

async fn history(
    request_id: RequestId,
    Extension(state): Extension<MailUiState>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let result = async {
        let Query(query) = query.map_err(|_| UiError::InvalidInput)?;
        let cursor = query.after.as_deref().map(canonical_uuid).transpose()?.map(CampaignId);
        let page = state.campaigns.list(cursor, PAGE_SIZE).await.map_err(|_| UiError::Unavailable)?;
        let readiness = SubscriberReadiness::load(&state.subscribers).await;
        let projection = state.publications.read();
        let posts = published_page(&projection, query.post_after.as_ref())?;
        Ok(admin_ui::page_response(StatusCode::OK, "Mail campaigns", PageKind::Authenticated, html! {
            h1 { "Mail campaigns" }
            (availability_panel(&state.access, &readiness))
            section class="panel" {
                h2 { "Review a published article" }
                p { "Each campaign uses the current public revision. Saving a draft does not send email." }
                (published_choices(&posts))
            }
            (campaign_history(&page))
        }))
    }.await;
    respond(request_id, result)
}

struct PublishedChoice<'a> {
    post_id: &'a PostId,
    title: &'a str,
}

fn published_page<'a>(
    projection: &'a PublicationReadProjection,
    after: Option<&PostId>,
) -> Result<Vec<PublishedChoice<'a>>, UiError> {
    projection
        .ledger
        .published_posts()
        .filter(|post| after.is_none_or(|after| post.post_id > *after))
        .take(PAGE_SIZE + 1)
        .map(|post| {
            let rendered = projection
                .catalog
                .get(&post.post_id, &post.revision)
                .ok_or(UiError::Unavailable)?;
            Ok(PublishedChoice {
                post_id: &post.post_id,
                title: rendered.document.metadata.title.as_str(),
            })
        })
        .collect()
}

fn published_choices(posts: &[PublishedChoice<'_>]) -> Markup {
    html! {
        @if posts.is_empty() { p class="muted" { "No more published articles." } }
        @for post in posts.iter().take(PAGE_SIZE) {
            p { a href=(format!("/admin/mail/posts/{}/review", post.post_id)) { (post.title) } " · " code { (post.post_id) } }
        }
        @if posts.len() > PAGE_SIZE {
            a href=(format!("/admin/mail?post_after={}", posts[PAGE_SIZE - 1].post_id)) { "More published articles" }
        }
    }
}

fn campaign_history(page: &CampaignPage) -> Markup {
    html! {
        section class="panel" {
            h2 { "Campaign history" }
            @if page.items.is_empty() { p class="muted" { "No more campaigns." } }
            @for campaign in &page.items {
                p {
                    a href=(campaign_path(campaign.campaign_id)) { (&campaign.content.subject) }
                    " — " (campaign.state.as_str()) " · " (timestamp(campaign.created_at))
                }
            }
            @if let Some(cursor) = page.next_cursor {
                a href=(format!("/admin/mail?after={}", cursor.0)) { "More campaigns" }
            }
        }
    }
}

async fn review(
    request_id: RequestId,
    Extension(state): Extension<MailUiState>,
    browser: BrowserFormSession,
    path: Result<Path<PostId>, axum::extract::rejection::PathRejection>,
) -> Response {
    let result = async {
        let Path(post_id) = path.map_err(|_| UiError::InvalidInput)?;
        let binding = state.access.review()?;
        let content = current_content(&state, &post_id)?;
        let readiness = SubscriberReadiness::load(&state.subscribers).await;
        Ok(admin_ui::page_response(
            StatusCode::OK,
            "Review announcement",
            PageKind::Authenticated,
            html! {
                h1 { "Review announcement" }
                (availability_panel(&state.access, &readiness))
                (content_preview(&content))
                (draft_form(&content, &binding.configuration_binding, browser.csrf_token.expose_secret(), browser.session.fresh_until > OffsetDateTime::now_utc()))
            },
        ))
    }.await;
    respond(request_id, result)
}

fn current_content(state: &MailUiState, post_id: &PostId) -> Result<CampaignContent, UiError> {
    let projection = state.publications.read();
    let published = projection
        .ledger
        .published_post(post_id)
        .ok_or(UiError::NotFound)?;
    let snapshot = state.snapshots.load_full();
    let announcement =
        Announcement::from_published(&projection, &snapshot, post_id, &published.revision)
            .map_err(UiError::Announcement)?;
    CampaignContent::from_announcement(announcement, projection.site.version)
        .map_err(|_| UiError::ReviewChanged)
}

async fn create(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<MailUiState>,
    path: Result<Path<PostId>, axum::extract::rejection::PathRejection>,
    form: Result<Form<ReviewForm>, FormRejection>,
) -> Response {
    let result = async {
        let Path(post_id) = path.map_err(|_| UiError::InvalidInput)?;
        let form = decode_form(form)?;
        let proposed_id = CampaignId(canonical_uuid(&form.proposed_id)?);
        let operation = AdminMutationKey(canonical_uuid(&form.idempotency_key)?);
        let (content, configuration_binding) = draft_content(&state, proposed_id, &post_id).await?;
        form.matches(&post_id, &content, &configuration_binding)?;
        state
            .campaigns
            .create_draft(CreateCampaign {
                proposed_id,
                content,
                configuration_binding,
                audit: principal.mutation_audit(request_id, operation),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(UiError::Mutation)
    }
    .await;
    mutation_response(request_id, result)
}

async fn draft_content(
    state: &MailUiState,
    id: CampaignId,
    post_id: &PostId,
) -> Result<(CampaignContent, [u8; 32]), UiError> {
    if let Some(saved) = state
        .campaigns
        .campaign(id)
        .await
        .map_err(|_| UiError::Unavailable)?
    {
        // Saved bytes permit receipt recovery after activation or configuration
        // changes. The sole writer still requires the exact original session
        // and fingerprint; a new operation cannot recreate an existing ID.
        return Ok((saved.content, saved.configuration_binding));
    }
    let binding = state.access.review()?;
    Ok((
        current_content(state, post_id)?,
        binding.configuration_binding,
    ))
}

async fn inspect(
    request_id: RequestId,
    Extension(state): Extension<MailUiState>,
    browser: BrowserFormSession,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Response {
    let result = async {
        let id = campaign_id(path)?;
        let campaign = load_campaign(&state.campaigns, id).await?;
        let readiness = SubscriberReadiness::load(&state.subscribers).await;
        Ok(admin_ui::page_response(StatusCode::OK, "Campaign", PageKind::Authenticated, html! {
            h1 { "Campaign" }
            p { a href="/admin/mail" { "Campaign history" } }
            p { "ID: " code { (id.0) } " · Version " (u64::from(campaign.version)) }
            p { "Created " (timestamp(campaign.created_at)) " · Updated " (timestamp(campaign.updated_at)) }
            (state_panel(&campaign.state))
            (content_preview(&campaign.content))
            (availability_panel(&state.access, &readiness))
            (campaign_controls(&campaign, &state.access, &readiness, browser.csrf_token.expose_secret(), browser.session.fresh_until > OffsetDateTime::now_utc()))
        }))
    }.await;
    respond(request_id, result)
}

async fn approve(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<MailUiState>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    form: Result<Form<ApproveForm>, FormRejection>,
) -> Response {
    let result = async {
        let id = campaign_id(path)?;
        let form = decode_form(form)?;
        let operation = AdminMutationKey(canonical_uuid(&form.idempotency_key)?);
        let campaign = load_campaign(&state.campaigns, id).await?;
        let readiness = if matches!(campaign.state, CampaignState::Draft) {
            SubscriberReadiness::load(&state.subscribers).await
        } else {
            SubscriberReadiness::Unavailable
        };
        let configuration_binding = state.access.approval_binding(&campaign, &readiness)?;
        if form.configuration_binding.as_ref() != hex(&configuration_binding) {
            return Err(UiError::ConfigurationChanged);
        }
        state
            .campaigns
            .approve(ApproveCampaign {
                campaign_id: id,
                expected_version: form.expected_version,
                configuration_binding,
                audit: principal.mutation_audit(request_id, operation),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(UiError::Mutation)
    }
    .await;
    mutation_response(request_id, result)
}

async fn cancel(
    request_id: RequestId,
    principal: AdminPrincipal,
    Extension(state): Extension<MailUiState>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    form: Result<Form<CancelForm>, FormRejection>,
) -> Response {
    let result = async {
        let id = campaign_id(path)?;
        let form = decode_form(form)?;
        let operation = AdminMutationKey(canonical_uuid(&form.idempotency_key)?);
        state
            .campaigns
            .cancel(CancelCampaign {
                campaign_id: id,
                expected_version: form.expected_version,
                audit: principal.mutation_audit(request_id, operation),
                now: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(UiError::Mutation)
    }
    .await;
    mutation_response(request_id, result)
}

fn campaign_id(
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
) -> Result<CampaignId, UiError> {
    let Path(value) = path.map_err(|_| UiError::InvalidInput)?;
    canonical_uuid(&value).map(CampaignId)
}

async fn load_campaign(store: &CampaignStore, id: CampaignId) -> Result<Campaign, UiError> {
    store
        .campaign(id)
        .await
        .map_err(|_| UiError::Unavailable)?
        .ok_or(UiError::NotFound)
}

fn decode_form<T>(form: Result<Form<T>, FormRejection>) -> Result<T, UiError> {
    form.map(|Form(value)| value)
        .map_err(|error| match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE => UiError::TooLarge,
            _ => UiError::InvalidInput,
        })
}

fn canonical_uuid(value: &str) -> Result<Uuid, UiError> {
    let id = Uuid::parse_str(value).map_err(|_| UiError::InvalidInput)?;
    if id.to_string() != value {
        return Err(UiError::InvalidInput);
    }
    Ok(id)
}

fn campaign_path(id: CampaignId) -> String {
    format!("/admin/mail/campaigns/{}", id.0)
}
fn hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*bytes).to_hex().to_string()
}
fn timestamp(value: OffsetDateTime) -> String {
    value
        .format(&Rfc3339)
        .unwrap_or_else(|_| "Time unavailable".to_owned())
}

fn availability_panel(access: &MailUiAccess, readiness: &SubscriberReadiness) -> Markup {
    html! {
        section class="panel" {
            h2 { "Mail availability" }
            @match access {
                MailUiAccess::Unavailable => { p class="notice" { "Mail configuration is unavailable. Existing campaigns can still be inspected or cancelled." } }
                MailUiAccess::ReviewOnly(binding) => {
                    p class="notice" { "Review only. Drafts can be saved; sending is not ready and approval is disabled." }
                    (provider_panel(binding))
                }
                MailUiAccess::DispatchReady(binding) => {
                    @if readiness.require_ready(&binding.configuration_binding).is_ok() {
                        p { "Sending is ready. Approving a draft authorizes this announcement to be sent." }
                    } @else {
                        p class="notice" { "Sending is not ready. New approvals are disabled until the active mail policy and delivery feedback are ready." }
                    }
                    (provider_panel(binding))
                }
            }
            (subscriber_panel(readiness))
        }
    }
}

fn subscriber_panel(readiness: &SubscriberReadiness) -> Markup {
    html! {
        h3 { "Subscriber status" }
        @match readiness {
            SubscriberReadiness::Unavailable => {
                p class="notice" { "Subscriber status and counts are unavailable. Cancellation remains available." }
            }
            SubscriberReadiness::Observed(status) => {
                @match status.policy {
                    None => { p { "Subscription controls have not been configured." } }
                    Some(policy) => {
                        @match policy.mode {
                            SubscriberMode::Paused => { p class="notice" { "New subscriptions and sending are paused." } }
                            SubscriberMode::Enabled => { p { "Mail policy is enabled." } }
                        }
                    }
                }
                @match status.feedback_health {
                    FeedbackHealth::Healthy => { p { "Delivery feedback is current." } }
                    FeedbackHealth::Unavailable => { p class="notice" { "Delivery feedback is unavailable or out of date. Sending waits for current feedback." } }
                    FeedbackHealth::ReconciliationRequired => {
                        p class="notice" { "Delivery feedback requires reconciliation. Sending stays paused until the operator resolves the missing or rejected feedback." }
                        p { a href="/admin/mail/recovery" { "Review feedback recovery" } }
                    }
                }
                @if let Some(last_ok) = status.last_feedback_ok_at {
                    p { "Last successful feedback check: " (timestamp(last_ok)) }
                }
                dl {
                    dt { "Confirmed subscriptions" } dd { (status.active_enrollments) }
                    dt { "Pending confirmations" } dd { (status.pending_enrollments) }
                    dt { "Stored email addresses" } dd { (status.addressed_enrollments) }
                    dt { "Retained enrollment records" } dd { (status.retained_enrollments) }
                }
            }
        }
    }
}

fn provider_panel(binding: &MailReviewBinding) -> Markup {
    let view = binding.configuration.view();
    html! {
        p { "Provider: Amazon SES · Sender: " (view.sender.as_str()) " · Region: " (view.region) }
        p { "Configuration set: " (view.configuration_set) }
        p { "Maximum recipients per campaign: " (view.max_campaign_recipients) " · Daily message budget: " (view.max_daily_messages) }
        @if let Some(policy)=view.subscriptions {
            @let policy=policy.view();
            dl {
                dt { "Publication operator" } dd { (policy.operator_name) }
                dt { "Postal address in email" } dd { (policy.postal_address) }
                dt { "Mailing purpose" } dd { (policy.purpose) }
                dt { "Public contact" } dd { (policy.contact_address.as_str()) }
                dt { "Privacy notice" } dd { a href=(policy.privacy_url.as_str()) { (policy.privacy_url.as_str()) } }
            }
        }
    }
}

fn content_preview(content: &CampaignContent) -> Markup {
    html! {
        section class="panel" {
            h2 { "Announcement preview" }
            p { "Subject: " strong { (&content.subject) } }
            p { "Public article: " a href=(&content.canonical_url) { (&content.canonical_url) } }
            p class="muted" { "Each recipient receives their own unsubscribe and removal links." }
            h3 { "Plain text" }
            pre { (&content.text) }
            details {
                summary { "HTML source" }
                // Persisted HTML is shown as text, never trusted as admin markup.
                pre { code { (&content.html) } }
            }
            details {
                summary { "Review identifiers" }
                p { "Reviewed revision: " code { (&content.revision) } }
                p { "Site version: " (content.site_version) " · Content digest: " code { (hex(&content.content_digest)) } }
            }
        }
    }
}

fn draft_form(content: &CampaignContent, binding: &[u8; 32], csrf: &str, fresh: bool) -> Markup {
    html! {
        section class="panel" {
            h2 { "Save draft" }
            (freshness_notice(fresh))
            form method="post" action=(format!("/admin/mail/posts/{}/review", content.post_id)) {
                input type="hidden" name="_csrf" value=(csrf);
                input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                input type="hidden" name="proposed_id" value=(Uuid::new_v4());
                input type="hidden" name="revision" value=(&content.revision);
                input type="hidden" name="snapshot" value=(&content.snapshot);
                input type="hidden" name="site_version" value=(content.site_version);
                input type="hidden" name="content_digest" value=(hex(&content.content_digest));
                input type="hidden" name="configuration_binding" value=(hex(binding));
                button type="submit" disabled[!fresh] { "Save reviewed draft" }
            }
        }
    }
}

fn campaign_controls(
    campaign: &Campaign,
    access: &MailUiAccess,
    readiness: &SubscriberReadiness,
    csrf: &str,
    fresh: bool,
) -> Markup {
    let can_cancel = matches!(
        campaign.state,
        CampaignState::Draft | CampaignState::Queued { .. } | CampaignState::Claimed { .. }
    );
    html! {
        section class="panel" {
            h2 { "Campaign controls" }
            (freshness_notice(fresh))
            @if matches!(campaign.state, CampaignState::Draft) {
                @if access.approval_binding(campaign, readiness).is_ok() {
                    p { "Approve only after reviewing the content and provider settings above. Provider acceptance does not prove inbox delivery." }
                    form method="post" action=(format!("{}/approve", campaign_path(campaign.campaign_id))) {
                        input type="hidden" name="_csrf" value=(csrf);
                        input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                        input type="hidden" name="expected_version" value=(u64::from(campaign.version));
                        input type="hidden" name="configuration_binding" value=(hex(&campaign.configuration_binding));
                        button type="submit" disabled[!fresh] { "Approve sending" }
                    }
                } @else { p class="notice" { "Approval is unavailable. Sending must be ready with the provider settings used for this draft." } }
            }
            @if can_cancel {
                p { "Cancellation stops further sending. Messages already being submitted may still be sent and cannot be recalled." }
                form method="post" action=(format!("{}/cancel", campaign_path(campaign.campaign_id))) {
                    input type="hidden" name="_csrf" value=(csrf);
                    input type="hidden" name="idempotency_key" value=(Uuid::new_v4());
                    input type="hidden" name="expected_version" value=(u64::from(campaign.version));
                    button type="submit" disabled[!fresh] { "Cancel campaign" }
                }
            }
        }
    }
}

fn freshness_notice(fresh: bool) -> Markup {
    html! { @if !fresh { p class="notice" { "Sign out and sign in again before changing this campaign. Your session is no longer fresh." } } }
}

fn state_panel(state: &CampaignState) -> Markup {
    html! {
        section class="panel" {
            h2 { "Status: " (state.as_str()) }
            @match state {
                CampaignState::Draft => { p { "This draft has not been approved." } }
                CampaignState::Queued { .. } => { p { "Approved and waiting to send. Delivery has not been confirmed." } }
                CampaignState::Claimed { .. } => { p { "This campaign is being sent. Final counts are not available yet." } }
                CampaignState::Cancelling { .. } => { p { "Cancellation was requested. Messages already being submitted may still be sent; final counts are not available yet." } }
                CampaignState::Completed { counts, .. } => { p { "Sending completed. Accepted means the provider accepted submission, not that a message reached an inbox." } (counts_panel(*counts)) }
                CampaignState::Cancelled { counts, .. } => { p { "This campaign is cancelled. Previously submitted messages cannot be recalled." } (counts_panel(*counts)) }
                CampaignState::Unknown { counts, .. } => { p class="notice" { "Some submission outcomes are unknown. Do not resend automatically; reconcile the original attempts first." } (counts_panel(*counts)) }
                CampaignState::Quarantined { progress, reason, .. } => {
                    @match reason {
                        CampaignQuarantine::Restore { .. } => { p class="notice" { "Sending is paused for review after restore. Restored campaigns are never automatically resumed." } }
                        CampaignQuarantine::FeedbackReset {..} => { p class="notice" { "The subscriber list was reset after a feedback gap. This campaign will not resume." } }
                        CampaignQuarantine::Interrupted => { p class="notice" { "Sending was interrupted and is paused for review. Check the original attempts before considering any further action." } }
                    }
                    @match progress {
                        CampaignProgress::Known(counts) => { (counts_panel(*counts)) }
                        CampaignProgress::Unreconciled => { p class="notice" { "Submission counts are unreconciled and unavailable. This does not mean zero messages were submitted." } }
                    }
                }
            }
        }
    }
}

fn counts_panel(counts: CampaignCounts) -> Markup {
    html! { dl { dt { "Provider accepted" } dd { (counts.accepted) } dt { "Rejected" } dd { (counts.rejected) } dt { "Unknown" } dd { (counts.unknown) } } }
}

fn respond(request_id: RequestId, result: Result<Response, UiError>) -> Response {
    result.unwrap_or_else(|error| error.response(request_id))
}

fn mutation_response(request_id: RequestId, result: Result<Campaign, UiError>) -> Response {
    respond(
        request_id,
        result.map(|campaign| admin_ui::redirect(&campaign_path(campaign.campaign_id))),
    )
}

#[derive(Debug, Error)]
enum UiError {
    #[error("the campaign form or route is invalid")]
    InvalidInput,
    #[error("the campaign form exceeds its byte bound")]
    TooLarge,
    #[error("the campaign or public article does not exist")]
    NotFound,
    #[error("campaign state is unavailable")]
    Unavailable,
    #[error("the reviewed content changed")]
    ReviewChanged,
    #[error("the reviewed provider configuration changed")]
    ConfigurationChanged,
    #[error("mail configuration is unavailable")]
    ConfigurationUnavailable,
    #[error("mail dispatch is unavailable")]
    DispatchUnavailable,
    #[error("prepare the public announcement")]
    Announcement(#[source] AnnouncementError),
    #[error("apply the campaign mutation")]
    Mutation(#[source] CampaignMutationError),
    #[error("apply the subscriber recovery mutation")]
    SubscriberMutation(#[source] SubscriberMutationError),
}

impl UiError {
    fn response(self, request_id: RequestId) -> Response {
        let (status, message) = self.description();
        admin_ui::error_response(
            status,
            "Mail campaign request not completed",
            message,
            request_id,
        )
    }

    fn description(self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidInput => (
                StatusCode::BAD_REQUEST,
                "The campaign form or address is invalid. Return to the campaign page and review the current values.",
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "The campaign form exceeds the request limit.",
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "The campaign or published article was not found.",
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Campaign state is unavailable. Retry this read before submitting another operation.",
            ),
            Self::ReviewChanged => (
                StatusCode::CONFLICT,
                "The reviewed public content changed. Review the current announcement before saving or approving it.",
            ),
            Self::ConfigurationChanged => (
                StatusCode::CONFLICT,
                "The provider settings changed. Cancel this draft and review a new one.",
            ),
            Self::ConfigurationUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Mail configuration is unavailable. Existing campaigns can still be inspected or cancelled.",
            ),
            Self::DispatchUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Sending is not ready. Approval remains unavailable until mail setup, the active policy, and delivery feedback are ready. Inspect the current mail status before retrying.",
            ),
            Self::Announcement(error) => announcement_error(error),
            Self::Mutation(error) => mutation_error(error),
            Self::SubscriberMutation(error) => recovery::mutation_error(error),
        }
    }
}

fn announcement_error(error: AnnouncementError) -> (StatusCode, &'static str) {
    match error {
        AnnouncementError::NotPublished => UiError::NotFound.description(),
        AnnouncementError::PublicationChanged => UiError::ReviewChanged.description(),
        AnnouncementError::RevisionUnavailable => UiError::Unavailable.description(),
        AnnouncementError::InvalidSubject
        | AnnouncementError::DescriptionTooLong
        | AnnouncementError::BodyTooLong => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "The published title or description cannot fit the announcement template. Correct and publish the article before reviewing it again.",
        ),
    }
}

fn mutation_error(error: CampaignMutationError) -> (StatusCode, &'static str) {
    match error {
        CampaignMutationError::Admission(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The campaign operation was not admitted. Retry the original form with its original operation ID.",
        ),
        CampaignMutationError::Command(command) => command_error(command),
    }
}

fn command_error(error: CampaignCommandError) -> (StatusCode, &'static str) {
    match error {
        CampaignCommandError::Forbidden => (
            StatusCode::FORBIDDEN,
            "A fresh Owner session is required. Sign out and sign in again before changing a campaign.",
        ),
        CampaignCommandError::ApprovalRevoked => (
            StatusCode::FORBIDDEN,
            "The approving account is no longer an enabled Owner.",
        ),
        CampaignCommandError::NotFound => UiError::NotFound.description(),
        CampaignCommandError::StaleVersion
        | CampaignCommandError::InvalidTransition
        | CampaignCommandError::StaleClaim => (
            StatusCode::CONFLICT,
            "The campaign changed. Inspect its current state before choosing another action.",
        ),
        CampaignCommandError::ActiveCampaign => (
            StatusCode::CONFLICT,
            "Another campaign is active. Inspect or cancel it before creating a draft.",
        ),
        CampaignCommandError::PublicationChanged => UiError::ReviewChanged.description(),
        CampaignCommandError::ConfigurationChanged => UiError::ConfigurationChanged.description(),
        CampaignCommandError::SendingUnavailable => UiError::DispatchUnavailable.description(),
        CampaignCommandError::InvalidValue => UiError::InvalidInput.description(),
        CampaignCommandError::Capacity => (
            StatusCode::CONFLICT,
            "The campaign history limit has been reached. Contact the server operator.",
        ),
        CampaignCommandError::IdempotencyConflict => (
            StatusCode::CONFLICT,
            "This operation ID belongs to a different form or browser session. Inspect campaign history before starting a new operation.",
        ),
        CampaignCommandError::OutcomeUnknown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The operation outcome is unknown. Inspect campaign history, or retry the original form in the same session with its original operation ID. Do not create another campaign to recover this request.",
        ),
    }
}

#[cfg(test)]
#[path = "ui/tests.rs"]
mod tests;
