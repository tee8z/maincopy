-- First-publication markers outlive recipient retention and never contain PII.
CREATE TABLE mail_article_notifications (
    post_id BLOB PRIMARY KEY CHECK(length(post_id)=16),
    publication_id BLOB NOT NULL CHECK(length(publication_id)=16)
) STRICT;

-- Upgrades must not mail the existing article archive.
INSERT INTO mail_article_notifications(post_id,publication_id)
SELECT stable_post_id,MIN(publication_id) FROM canonical_publications
WHERE published_at_ns IS NOT NULL GROUP BY stable_post_id;

DROP INDEX mail_campaign_active_idx;
CREATE UNIQUE INDEX mail_campaign_active_idx ON mail_campaigns((1))
WHERE state IN ('claimed','cancelling');
CREATE UNIQUE INDEX mail_campaign_draft_idx ON mail_campaigns((1)) WHERE state='draft';

-- Only explicit provider rejection may return an attempt to the waiting queue.
ALTER TABLE mail_attempts ADD COLUMN retry_after INTEGER NOT NULL DEFAULT 0 CHECK(retry_after>=0);
