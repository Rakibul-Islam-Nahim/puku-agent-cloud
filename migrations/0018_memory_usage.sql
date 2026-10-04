-- F28: make memory traffic observable from the control plane.
--
-- The integration adds a call to the memory service at every session start and
-- every session end, and a slow or failing dependency there shows up as boot
-- latency long before anyone thinks to look at it.
-- `usage_records` cannot carry this: it is keyed one row per session, while
-- memory traffic is per-org and per-day, and consolidation has no session at
-- all.
--
-- The memory service tracks model spend in its own `spend_ledger`, which is
-- the only place that knows token counts. This tracks what agent-cloud caused:
-- traffic between the two services. Neither figure is a Cloudflare bill --
-- extraction is local and serving retrieves nothing.
CREATE TABLE IF NOT EXISTS memory_usage (
    org_id          uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    period          date NOT NULL,
    recalls         bigint NOT NULL DEFAULT 0,
    recall_failures bigint NOT NULL DEFAULT 0,
    -- Cumulative, so mean latency is derivable without a histogram here; the
    -- distribution lives in the memory service's own metrics.
    recall_ms       bigint NOT NULL DEFAULT 0,
    ingests         bigint NOT NULL DEFAULT 0,
    ingest_failures bigint NOT NULL DEFAULT 0,
    messages_sent   bigint NOT NULL DEFAULT 0,
    preamble_bytes  bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (org_id, period)
);
