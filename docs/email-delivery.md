# Mailing-list privacy and dispatch plan

Status: active first-release work; subscriber capture and sending remain disabled.

Last reviewed: 2026-09-06

Related: [system design](design.md), [implementation plan](implementation.md), and
[engineering style](quality.md).

## Selected delivery provider

Use Amazon Simple Email Service (SES) for the first email implementation.
The owner selected SES for its usage-based price and expects fewer than 2,000 subscribers for the foreseeable future.
DynamoDB is excluded. SES selection does not introduce another AWS database.

Implement email before home-server deployment. Enable capture and sending only after
privacy, recovery, dispatch, and deliverability acceptance passes together.
Local fixtures do not establish provider behavior or deployment acceptance.

The first campaign announces one explicitly published article revision. An owner
reviews and authorizes its email separately from website publication.
Automatic X, Substack, and Nostr delivery remain outside this increment.

Current work includes public announcement preparation and the SES transport and
campaign-storage implementation. A working subscription or removal service is not complete.

## Cost and operating scope

SES a-la-carte outbound delivery costs $0.10 per 1,000 recipient messages.
At 2,000 subscribers and four newsletters per month, 8,000 deliveries cost $0.80 before additional charges.
Message data, confirmation emails, event processing, and optional services add to that amount.
These estimates exclude taxes and temporary credits. Pricing was checked on 2026-09-06.
Select à-la-carte pricing during setup. New accounts can default to Essentials,
which currently charges $0.16 per 1,000 emails.
[AWS SES pricing](https://aws.amazon.com/ses/pricing/).

Keep optional paid services disabled unless the deployment needs them.
Use explicit sending budgets and the account's actual quotas. Do not assume
production access, a dedicated IP address, or a paid deliverability package.
A provider price advantage does not remove consent, erasure, or recovery obligations.

Brevo and Mailchimp remain possible later adapters. They are not required for this deployment.
Their hosted campaign workflows differ from SES recipient sending; switching requires a concrete migration design.

## Data ownership and storage decision

Treat subscriber addresses and linkable identifiers as personally identifiable information (PII).
Keep subscriber addresses, contact hashes, consent tokens, recipient lists, and raw
subscriber events out of the existing site SQLite database, WAL, Git content, and B2 checkpoints.

The proposed subscriber authority is a separate SQLite database on the home server.
Its location, access protection, deletion behavior, and independent encrypted backup
and recovery policy must be settled before enabling capture.
The owner is reviewing local storage versus an external subscriber authority.
Do not silently include this database in the existing Litestream capture or site export.
Without a separate backup, loss of this database also loses the mailing-list state.

The site database may retain reviewed public campaign content, sender configuration
bindings, owner authorization, campaign identities, and aggregate dispatch outcomes.
Use typed commands through its sole writer. Keep subscriber records and individual
recipient state within the selected subscriber authority.

Use protected Maincopy-owned buffers for transient addresses, credentials, control
tokens, and provider payloads. Bound input and redact diagnostics.
Do not put them in logs, traces, metrics, audit text, or error messages.
Framework JSON, HTTP, TLS, and AWS signing allocations have separate lifetimes;
complete memory wiping is not guaranteed.

SES necessarily processes recipient addresses and message content during delivery.
Document provider retention and suppression separately. Do not describe removal from
Maincopy as instant deletion of every provider or backup copy.

## SES transport boundary

Start with one concrete SES adapter with inherent methods. Add other concrete
adapters when their production implementations exist; use a closed enum when selecting among them.
Do not add a plugin framework or a speculative project trait.

Keep public announcement rendering, owner approval, consent, cancellation, and
recovery policy in Maincopy's mail domain. The SES adapter owns request validation,
AWS request signing, bounded HTTPS, and typed provider outcomes.
Use protected runtime credentials with a fixed regional endpoint.
Disable automatic transport retries for non-idempotent sends.

Each approved campaign binds the public revision, rendered bytes, template version,
subject, sender, provider configuration, audience policy, and authorization.
A configuration change affects new approvals. Existing work retains its original binding.
Do not reroute uncertain submissions through another provider.

SES contact management is not the consent or delivery ledger for this design.
Sending with `ListManagementOptions` can create an absent contact automatically.
The adapter must omit this field so a stale send cannot recreate a deleted SES contact.
[SES list management](https://docs.aws.amazon.com/ses/latest/dg/sending-email-list-management.html).

The contact APIs do not establish conditional consent updates or strongly consistent
recovery reads. `UpdateContact` also requires resupplying existing topic preferences.
Do not use contact attributes as a replacement for durable admission and deletion state.
[UpdateContact](https://docs.aws.amazon.com/ses/latest/APIReference-V2/API_UpdateContact.html),
[SES consistency guidance](https://docs.aws.amazon.com/ses/latest/dg/troubleshoot-general.html).

## Consent, unsubscribe, and removal

Maincopy owns the public signup and confirmation flow. Publish the mailing purpose,
operator identity, privacy notice, contact channel, and retention policy before capture.
Require double opt-in before newsletter eligibility. Do not infer consent from an
SES contact, successful send response, imported address, or restored record.
[AWS sending practices](https://docs.aws.amazon.com/ses/latest/dg/tips-and-best-practices.html).

Bound repeated signup and confirmation attempts. Use generic responses to resist
enumeration and list bombing. Expire unconfirmed requests and their unnecessary PII.
Confirmation tokens must bind their purpose, site, enrollment generation, nonce, and expiry.
Consume confirmation state atomically. A signed token alone is not single-use.

Every newsletter needs a visible **Unsubscribe and remove my address** control.
It must stop future admission and perform the requested cleanup, with accurate
pending, unknown, and completion states. Do not require a Maincopy account or send a goodbye email.
Fresh enrollment must create new consent and invalidate controls for the old generation.

Support [RFC 8058 one-click unsubscribe](https://www.rfc-editor.org/rfc/rfc8058.html).
Its authenticated HTTPS `POST` must work without login, cookies, redirects, or
additional confirmation. Verify DKIM coverage for both unsubscribe headers in real mail.
Unsubscribe controls must remain usable after a short confirmation-token lifetime expires.

Browser `GET` requests only display a confirmation page; they must not change consent.
Use an explicit browser action for removal, restrictive CSP, `no-store`, and `no-referrer`.
Keep tokens out of access logs and third-party resources.

The subscriber authority must serialize consent revocation with recipient admission.
A removal request blocks new admission and cancels work not yet admitted.
Previously admitted or delivered messages cannot be reliably recalled; state this boundary clearly.
A delayed cleanup attempt must not erase a fresh enrollment accidentally.

Treat complaints and hard bounces as suppression events. Authenticate feedback,
handle duplicates and reordering, and keep raw recipient payloads out of the site ledger.
Do not remove suppression merely to make another send succeed.

SES account-level suppression and internal global suppression are separate from
contact membership. Account entries can persist until removed; global hard-bounce
entries can persist for up to 14 days without a customer deletion interface.
These facts limit any promise of complete provider erasure.
[Account suppression](https://docs.aws.amazon.com/ses/latest/dg/sending-email-suppression-list.html),
[Global suppression](https://docs.aws.amazon.com/ses/latest/dg/sending-email-global-suppression-list.html).

## Durable campaign dispatch

Only an owner with fresh browser authentication may create or authorize a campaign.
Publishers and current agent scopes gain no email authority implicitly.
Recheck current authority and the public revision inside the sole-writer transaction.
Never select a private preview, draft, or later unreviewed revision.

Persist the reviewed content and a unique operation identity before provider work.
Use bounded claims, leases, fencing, and typed transitions. Keep provider calls
outside database transactions. A lease expiry must not authorize a duplicate send.

The initial campaign workflow permits one active campaign. Distinguish drafts,
queued work, claimed work, cancellation in progress, completion, unknown outcomes,
and quarantine. Keep aggregate accepted, rejected, and unknown counts accurate.
Provider acceptance does not prove inbox delivery.

SES sends to supplied recipients rather than accepting a durable provider campaign.
The recipient dispatcher therefore needs durable progress and current consent checks
in the subscriber authority. Define the audience cutoff without copying recipients into site checkpoints.
[SES SendEmail](https://docs.aws.amazon.com/ses/latest/APIReference-V2/API_SendEmail.html).

Commit recipient admission before transmission. Bind it to the campaign approval,
configuration, current consent generation, and a unique attempt identity.
Honor cancellation and sending budgets at admission. Record the outcome afterward.

SES `SendEmail` has no documented client idempotency token. A timeout can mean the
provider accepted the message while the response was lost.
Record that outcome as unknown and do not resend automatically.
Correlated provider events may supply positive acceptance evidence; silence is not proof of rejection.
[SES event publishing](https://docs.aws.amazon.com/ses/latest/dg/monitor-using-event-publishing.html).

The application owns, supervises, cancels, and awaits mail workers.
Shutdown closes ingress, stops new claims, drains accepted work, and records unresolved outcomes.
Provider failures must not block public reading or roll back website publication.

## Backup and restore protection

Existing site checkpoints must contain no subscriber records, recipient lists, or control tokens.
Verify the boundary across SQLite, WAL, retained content, diagnostics, and exported bundles.
The separate subscriber authority needs its own deletion and recovery acceptance.

Quarantine unfinished campaigns in the offline site-restore acceptance transaction,
before an accepted-restore marker can enable startup. Also quarantine interrupted
claims before a new worker admits work after a restart.
Preserve completed receipts without replaying them.

A site restore must not overwrite current subscriber consent, replay confirmation
requests, or restart old sends. Resuming mail requires current subscriber state and
explicit owner review. A restored subscriber copy must not revive removed consent.
Keep capture disabled until the subscriber backup policy proves these properties.

## Deployment and deliverability acceptance

Complete this procedure with the selected SES account and authorized test recipients:

1. Select the AWS region, sending domain, visible sender, and monitored reply address.
2. Verify the SES identity and configure Easy DKIM with the provider's exact DNS records.
3. Configure a custom MAIL FROM domain and its required SPF records when selected.
4. Publish DMARC and verify alignment for the visible sender domain.
5. Select à-la-carte pricing and disable unneeded paid options.
6. Request production access and record actual sending quotas and rate limits.
7. Create a narrowly scoped runtime credential and protect it outside Git and backups.
8. Configure authenticated bounce, complaint, and delivery-event handling with bounded retention.
9. Verify that effective suppression includes both `BOUNCE` and `COMPLAINT`, without a configuration-set override that disables either.
10. Disable open and click tracking in every event destination and optional engagement service.
11. Inspect delivered HTML, plain text, sender information, and DKIM-covered unsubscribe headers.
12. Test confirmation, one-click unsubscribe, removal, suppression, restart, and restore with controlled recipients.
13. Start with a small confirmed audience and verify budgets and the campaign pause mechanism.

Check the effective configuration, including overrides, against
[SES suppression settings](https://docs.aws.amazon.com/ses/latest/dg/sending-email-suppression-list.html).
SES open and click tracking modifies messages and collects recipient activity.
[SES tracking behavior](https://docs.aws.amazon.com/ses/latest/dg/configure-custom-open-click-domains.html).

Do not send tests or campaigns without owner authorization for the recipients and launch.
Record redacted DNS, authentication, control, and recovery evidence.
Authentication and list hygiene reduce blocking risk; they cannot guarantee inbox placement.

Use the receiving services' current sender requirements when configuring the account:
[Google sender guidelines](https://support.google.com/a/answer/81126),
[Yahoo sender requirements](https://senders.yahooinc.com/best-practices/), and
[Microsoft high-volume sender requirements](https://techcommunity.microsoft.com/blog/microsoftdefenderforoffice365blog/strengthening-email-ecosystem-outlook%E2%80%99s-new-requirements-for-high%E2%80%90volume-senders/4399730).

## Inclusion gate

Exercise confirmation expiry and replay, enumeration resistance, removal during every
dispatch stage, duplicate and reordered feedback, provider outages, restart, and older-backup restoration.
Verify PII redaction, cleanup, suppression, and no automatic retry of unknown sends.

If any required check remains incomplete, release core V1 with subscriptions and sending disabled.
Do not ship address capture alone.
