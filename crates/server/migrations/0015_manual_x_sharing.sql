-- X is shared by hand from the admin page. Its API channel, stored keys, and
-- deliveries are removed; SQLite needs a rebuilt table to narrow the channel.
DROP TABLE sharing_x;

CREATE TABLE sharing_deliveries_next (
    post_id BLOB NOT NULL CHECK (length(post_id) = 16) REFERENCES sharing_teasers(post_id),
    channel TEXT NOT NULL CHECK (channel IN ('substack')),
    state TEXT NOT NULL CHECK (state IN ('queued','sending','posted','failed')),
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 1000),
    retry_after INTEGER NOT NULL CHECK (retry_after >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    draft INTEGER CHECK (draft IS NULL OR draft > 0),
    posted_url TEXT CHECK (posted_url IS NULL OR length(CAST(posted_url AS BLOB)) BETWEEN 1 AND 2048),
    failure TEXT CHECK (failure IS NULL OR failure IN ('interrupted','refused','unexpected_response','retries_exhausted')),
    PRIMARY KEY (post_id, channel),
    CHECK ((state = 'posted') = (posted_url IS NOT NULL)),
    CHECK ((state = 'failed') = (failure IS NOT NULL))
) STRICT;

INSERT INTO sharing_deliveries_next
SELECT post_id,channel,state,attempts,retry_after,updated_at,draft,posted_url,failure
FROM sharing_deliveries WHERE channel = 'substack';

DROP TABLE sharing_deliveries;
ALTER TABLE sharing_deliveries_next RENAME TO sharing_deliveries;
