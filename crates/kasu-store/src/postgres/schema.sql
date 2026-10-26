CREATE SCHEMA IF NOT EXISTS kasu;
CREATE TABLE IF NOT EXISTS kasu.schema_version (singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton), version BIGINT NOT NULL);
INSERT INTO kasu.schema_version VALUES (TRUE,1) ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS kasu.immutable (id BYTEA PRIMARY KEY CHECK(octet_length(id)=32), key BYTEA NOT NULL, value BYTEA NOT NULL);
CREATE TABLE IF NOT EXISTS kasu.repositories (id BYTEA PRIMARY KEY CHECK(octet_length(id)=32), name BYTEA NOT NULL);
CREATE TABLE IF NOT EXISTS kasu.refs (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), id BYTEA NOT NULL CHECK(octet_length(id)=32),
    kind SMALLINT NOT NULL CHECK(kind IN (0,1)), name BYTEA NOT NULL, value BYTEA,
    version BIGINT NOT NULL CHECK(version>=0), PRIMARY KEY(repo,kind,id)
);
CREATE TABLE IF NOT EXISTS kasu.log_counters (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), stream SMALLINT NOT NULL CHECK(stream IN (0,1)),
    value BIGINT NOT NULL CHECK(value>=0), PRIMARY KEY(repo,stream)
);
CREATE TABLE IF NOT EXISTS kasu.ref_log (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), stream SMALLINT NOT NULL CHECK(stream IN (0,1)),
    seq BIGINT NOT NULL CHECK(seq>0), name BYTEA NOT NULL, old BYTEA, new BYTEA, actor BYTEA NOT NULL, time BIGINT NOT NULL,
    PRIMARY KEY(repo,stream,seq)
);
CREATE TABLE IF NOT EXISTS kasu.land_claims (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), key BYTEA NOT NULL, PRIMARY KEY(repo,key)
);
CREATE TABLE IF NOT EXISTS kasu.land_results (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), key BYTEA NOT NULL,
    fingerprint BYTEA NOT NULL CHECK(octet_length(fingerprint)=32), response BYTEA NOT NULL, PRIMARY KEY(repo,key)
);
CREATE TABLE IF NOT EXISTS kasu.webhook_state (
    repo BYTEA NOT NULL REFERENCES kasu.repositories(id), consumer TEXT NOT NULL, stream SMALLINT NOT NULL CHECK(stream IN (0,1)),
    cursor BIGINT NOT NULL DEFAULT 0 CHECK(cursor>=0), sequence BIGINT, body BYTEA,
    attempts BIGINT NOT NULL DEFAULT 0 CHECK(attempts>=0), retry_at_ms BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY(repo,consumer,stream),
    CHECK ((sequence IS NULL AND body IS NULL) OR (sequence>cursor AND body IS NOT NULL))
);
