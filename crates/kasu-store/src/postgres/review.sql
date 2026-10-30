ALTER TABLE kasu.ref_log ADD COLUMN IF NOT EXISTS via BYTEA;
CREATE TABLE IF NOT EXISTS kasu.review_counters (
    repo BYTEA PRIMARY KEY REFERENCES kasu.repositories(id), next BIGINT NOT NULL CHECK(next>0)
);
CREATE TABLE IF NOT EXISTS kasu.reviews (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), number BIGINT NOT NULL CHECK(number>0),
    version BIGINT NOT NULL CHECK(version>0), state SMALLINT NOT NULL CHECK(state BETWEEN 0 AND 2),
    target BYTEA NOT NULL, head BYTEA NOT NULL, author TEXT NOT NULL, data BYTEA NOT NULL,
    PRIMARY KEY(repo,number)
);
CREATE INDEX IF NOT EXISTS reviews_state ON kasu.reviews(repo,state,number DESC);
CREATE INDEX IF NOT EXISTS reviews_target ON kasu.reviews(repo,target,state);
CREATE INDEX IF NOT EXISTS reviews_author ON kasu.reviews(repo,author,state);
CREATE TABLE IF NOT EXISTS kasu.review_revisions (
    repo BYTEA NOT NULL, number BIGINT NOT NULL, revision BIGINT NOT NULL CHECK(revision>0), data BYTEA NOT NULL,
    PRIMARY KEY(repo,number,revision), FOREIGN KEY(repo,number) REFERENCES kasu.reviews(repo,number)
);
CREATE TABLE IF NOT EXISTS kasu.review_votes (
    repo BYTEA NOT NULL, number BIGINT NOT NULL, account TEXT NOT NULL, data BYTEA NOT NULL,
    PRIMARY KEY(repo,number,account), FOREIGN KEY(repo,number) REFERENCES kasu.reviews(repo,number)
);
CREATE TABLE IF NOT EXISTS kasu.review_comments (
    repo BYTEA NOT NULL, number BIGINT NOT NULL, id BIGINT NOT NULL CHECK(id>0), author TEXT NOT NULL,
    draft BOOLEAN NOT NULL, data BYTEA NOT NULL,
    PRIMARY KEY(repo,number,id), FOREIGN KEY(repo,number) REFERENCES kasu.reviews(repo,number)
);
CREATE TABLE IF NOT EXISTS kasu.review_submissions (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), account TEXT NOT NULL, key BYTEA NOT NULL,
    fingerprint BYTEA NOT NULL, response BYTEA NOT NULL, PRIMARY KEY(repo,account,key)
);
CREATE TABLE IF NOT EXISTS kasu.review_log (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), seq BIGSERIAL, number BIGINT NOT NULL,
    time BIGINT NOT NULL, data BYTEA NOT NULL, PRIMARY KEY(repo,seq)
);
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'webhook_state_streams') THEN
    ALTER TABLE kasu.webhook_state DROP CONSTRAINT IF EXISTS webhook_state_stream_check;
    ALTER TABLE kasu.webhook_state ADD CONSTRAINT webhook_state_streams CHECK(stream IN (0,1,2));
  END IF;
END $$;
