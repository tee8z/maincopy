use markdown_compiler::{PostId, PostRevisionDigest, SiteSnapshotDigest};
use maud::html;
use thiserror::Error;

use crate::{
    domain::publication::{CanonicalSiteUrl, activation::PublicationReadProjection},
    render::SiteSnapshot,
};

pub(super) const ANNOUNCEMENT_TEMPLATE_VERSION: u32 = 1;
// Leave room for escaped recipient controls inside the transport's body limit.
pub(super) const MAX_ANNOUNCEMENT_BODY_BYTES: usize = 64 * 1024;
const MAX_SUBJECT_BYTES: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

/// Common campaign bytes contain published content only. Recipient addresses
/// and control links are added in protected memory at submission time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Announcement {
    pub post_id: PostId,
    pub revision: PostRevisionDigest,
    pub snapshot: SiteSnapshotDigest,
    pub canonical_url: CanonicalSiteUrl,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub content_digest: [u8; 32],
}

impl Announcement {
    /// Select the durable public revision, never the catalog's latest private
    /// revision. The two immutable views must represent the same publication.
    pub(super) fn from_published(
        publication: &PublicationReadProjection,
        snapshot: &SiteSnapshot,
        post_id: &PostId,
        expected_revision: &PostRevisionDigest,
    ) -> Result<Self, AnnouncementError> {
        if publication.site.digest != snapshot.digest {
            return Err(AnnouncementError::PublicationChanged);
        }
        let published = publication
            .ledger
            .published_post(post_id)
            .ok_or(AnnouncementError::NotPublished)?;
        if &published.revision != expected_revision {
            return Err(AnnouncementError::PublicationChanged);
        }
        let post = publication
            .catalog
            .get(post_id, expected_revision)
            .ok_or(AnnouncementError::RevisionUnavailable)?;
        let canonical_url = snapshot
            .post_canonical_url(&post.document.metadata.slug)
            .ok_or(AnnouncementError::RevisionUnavailable)?
            .clone();
        let subject = post.document.metadata.title.as_str();
        let description = post.document.metadata.description.as_str();
        validate_subject(subject)?;
        if description.len() > MAX_DESCRIPTION_BYTES {
            return Err(AnnouncementError::DescriptionTooLong);
        }
        let text = format!(
            "{subject}\n\n{description}\n\nRead the article: {}\n",
            canonical_url.as_str()
        );
        let html = html! {
            h1 { (subject) }
            p { (description) }
            p { a href=(canonical_url.as_str()) { "Read the article" } }
        }
        .into_string();
        if text.len() > MAX_ANNOUNCEMENT_BODY_BYTES || html.len() > MAX_ANNOUNCEMENT_BODY_BYTES {
            return Err(AnnouncementError::BodyTooLong);
        }
        let content_digest = announcement_content_digest(
            post_id,
            expected_revision,
            canonical_url.as_str(),
            subject,
            &text,
            &html,
        );
        Ok(Self {
            post_id: post_id.clone(),
            revision: expected_revision.clone(),
            snapshot: snapshot.digest.clone(),
            canonical_url,
            subject: subject.into(),
            text,
            html,
            content_digest,
        })
    }
}

/// Both preparation and persisted-content validation use the template's exact
/// length-framed identity. Adding recipient controls must not change it.
pub(super) fn announcement_content_digest(
    post_id: &PostId,
    revision: &PostRevisionDigest,
    canonical_url: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> [u8; 32] {
    let mut digest = blake3::Hasher::new();
    digest.update(b"maincopy announcement template v1\0");
    for value in [
        post_id.as_str().as_bytes(),
        revision.as_str().as_bytes(),
        canonical_url.as_bytes(),
        subject.as_bytes(),
        text.as_bytes(),
        html.as_bytes(),
    ] {
        digest.update(&(value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    *digest.finalize().as_bytes()
}

fn validate_subject(subject: &str) -> Result<(), AnnouncementError> {
    if subject.is_empty()
        || subject.len() > MAX_SUBJECT_BYTES
        || subject.chars().any(char::is_control)
    {
        return Err(AnnouncementError::InvalidSubject);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum AnnouncementError {
    #[error("the article has no published revision")]
    NotPublished,
    #[error("the public revision changed; review the current announcement")]
    PublicationChanged,
    #[error("the published revision is unavailable")]
    RevisionUnavailable,
    #[error("the article title cannot be used as an email subject")]
    InvalidSubject,
    #[error("the article description exceeds the email template limit")]
    DescriptionTooLong,
    #[error("the rendered announcement exceeds the email template limit")]
    BodyTooLong,
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use markdown_compiler::{ContentTreeDigest, PostCollection, prepare_content};
    use time::OffsetDateTime;

    use super::*;
    use crate::{
        content_fixtures::{content_tree, post, publication},
        domain::publication::{PublicLedgerProjection, PublishedPostRevision, store::SiteHead},
        frontend_assets::embedded_manifest,
        render::{compile_content_catalog, render_site_shell},
    };

    const PUBLIC_ARTICLE: &str = "+++\nid = \"11111111-1111-4111-8111-111111111111\"\n\
        title = \"A <reviewed> article\"\nslug = \"article\"\n\
        authored_at = 2026-09-01T00:00:00Z\n\
        description = \"A <script> description & summary.\"\n\
        draft = false\n+++\n\nArticle body.\n";

    fn fixture(
        published: bool,
        article: &str,
    ) -> (
        PublicationReadProjection,
        SiteSnapshot,
        PostId,
        PostRevisionDigest,
    ) {
        let tree = content_tree(
            publication(
                "publication.toml",
                "[site]\ntitle = \"Mail fixture\"\nbase_url = \"https://example.com\"\n\
                 description = \"A public site\"\n[author]\nname = \"Writer\"\n"
                    .into(),
            ),
            vec![post(
                "posts/article.md",
                PostCollection::Posts,
                article.into(),
            )],
            vec![],
            0,
        );
        let catalog = Arc::new(compile_content_catalog(&prepare_content(&tree).unwrap()).unwrap());
        let rendered = catalog.rendered_posts().next().unwrap();
        let post_id = rendered.document.metadata.id.clone();
        let revision = rendered.revision.clone();
        let ledger = if published {
            PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
                post_id.clone(),
                revision.clone(),
                OffsetDateTime::from_unix_timestamp(1_788_652_800).unwrap(),
            )])
            .unwrap()
        } else {
            PublicLedgerProjection::empty()
        };
        let snapshot = render_site_shell(Arc::clone(&catalog), embedded_manifest(), &ledger)
            .unwrap()
            .into_snapshot()
            .unwrap();
        let projection = PublicationReadProjection {
            catalog,
            content_digest: ContentTreeDigest::from_bytes([1; 32]),
            candidates: Arc::new(BTreeMap::new()),
            ledger,
            site: SiteHead {
                digest: snapshot.digest.clone(),
                version: 1,
            },
            frontend: embedded_manifest(),
            tip_recipient: None,
        };
        (projection, snapshot, post_id, revision)
    }

    #[test]
    fn announcement_requires_explicit_publication_and_coherent_public_views() {
        let (private, snapshot, post_id, revision) = fixture(false, PUBLIC_ARTICLE);
        assert_eq!(
            Announcement::from_published(&private, &snapshot, &post_id, &revision),
            Err(AnnouncementError::NotPublished)
        );
        let (published, public_snapshot, _, _) = fixture(true, PUBLIC_ARTICLE);
        assert_eq!(
            Announcement::from_published(&published, &snapshot, &post_id, &revision),
            Err(AnnouncementError::PublicationChanged)
        );
        let announcement =
            Announcement::from_published(&published, &public_snapshot, &post_id, &revision)
                .unwrap();
        assert_eq!(
            announcement.canonical_url.as_str(),
            "https://example.com/posts/article"
        );
        assert_eq!(announcement.subject, "A <reviewed> article");
        assert!(announcement.html.contains("&lt;reviewed&gt;"));
        assert!(announcement.html.contains("&lt;script&gt;"));
        assert!(!announcement.html.contains("<script>"));
        assert!(!announcement.html.contains("/preview"));
        assert!(
            announcement
                .text
                .contains("A <script> description & summary.")
        );
        assert_eq!(
            announcement,
            Announcement::from_published(&published, &public_snapshot, &post_id, &revision)
                .unwrap()
        );
    }

    #[test]
    fn announcements_use_retained_public_content_and_reject_private_or_stale_revisions() {
        let (published, published_snapshot, post_id, published_revision) =
            fixture(true, PUBLIC_ARTICLE);
        let approved = Announcement::from_published(
            &published,
            &published_snapshot,
            &post_id,
            &published_revision,
        )
        .unwrap();
        let (mut current, _, _, private_revision) = fixture(
            false,
            "+++\nid = \"11111111-1111-4111-8111-111111111111\"\n\
             title = \"PRIVATE unreviewed title\"\nslug = \"article\"\n\
             authored_at = 2026-09-01T00:00:00Z\n\
             updated_at = 2026-09-02T00:00:00Z\n\
             description = \"PRIVATE unreviewed description\"\n\
             draft = false\n+++\n\nPRIVATE unreviewed body.\n",
        );
        assert_ne!(private_revision, published_revision);
        current.ledger = published.ledger.clone();
        Arc::make_mut(&mut current.catalog)
            .retain_revisions_from(&published.catalog, current.ledger.revision_keys())
            .unwrap();
        assert_eq!(
            current.catalog.current_post(&post_id).unwrap().revision,
            private_revision
        );
        let public_snapshot = render_site_shell(
            Arc::clone(&current.catalog),
            embedded_manifest(),
            &current.ledger,
        )
        .unwrap()
        .into_snapshot()
        .unwrap();
        current.site.digest = public_snapshot.digest.clone();

        let announcement =
            Announcement::from_published(&current, &public_snapshot, &post_id, &published_revision)
                .unwrap();
        assert_eq!(announcement, approved);
        assert_eq!(
            Announcement::from_published(&current, &public_snapshot, &post_id, &private_revision),
            Err(AnnouncementError::PublicationChanged)
        );

        // Only a subsequent explicit publication makes the newer revision
        // available to announcements and invalidates the previous approval.
        current.ledger =
            PublicLedgerProjection::try_from_exact_entries([PublishedPostRevision::new(
                post_id.clone(),
                private_revision.clone(),
                OffsetDateTime::from_unix_timestamp(1_788_739_200).unwrap(),
            )])
            .unwrap();
        let updated_snapshot = render_site_shell(
            Arc::clone(&current.catalog),
            embedded_manifest(),
            &current.ledger,
        )
        .unwrap()
        .into_snapshot()
        .unwrap();
        current.site.digest = updated_snapshot.digest.clone();
        current.site.version += 1;
        assert_eq!(
            Announcement::from_published(
                &current,
                &updated_snapshot,
                &post_id,
                &published_revision,
            ),
            Err(AnnouncementError::PublicationChanged)
        );
        let updated =
            Announcement::from_published(&current, &updated_snapshot, &post_id, &private_revision)
                .unwrap();
        assert_eq!(updated.subject, "PRIVATE unreviewed title");
        assert!(updated.text.contains("PRIVATE unreviewed description"));
        assert!(updated.html.contains("PRIVATE unreviewed description"));
        assert_ne!(updated.content_digest, approved.content_digest);
    }

    #[test]
    fn subjects_reject_header_injection_and_bound_encoded_bytes() {
        assert_eq!(validate_subject("A published article"), Ok(()));
        for subject in [
            "",
            "Title\r\nBcc: recipient@example.test",
            "Title\n",
            "Title\0",
        ] {
            assert_eq!(
                validate_subject(subject),
                Err(AnnouncementError::InvalidSubject)
            );
        }
        assert_eq!(validate_subject(&"a".repeat(MAX_SUBJECT_BYTES)), Ok(()));
        assert_eq!(
            validate_subject(&"é".repeat(MAX_SUBJECT_BYTES)),
            Err(AnnouncementError::InvalidSubject)
        );
    }
}
