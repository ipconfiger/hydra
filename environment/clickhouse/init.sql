-- ClickHouse init: create usage_record table for Hydra's ClickHouseSink.
-- Auto-run on first ClickHouse start via /docker-entrypoint-initdb.d/.
CREATE TABLE IF NOT EXISTS usage_record (
    tenant_id          String,
    provider_id        String,
    model_key          String,
    client_api_key     Nullable(String),
    status_code        UInt16,
    prompt_tokens      Nullable(UInt64),
    completion_tokens  Nullable(UInt64),
    total_tokens       Nullable(UInt64),
    latency_ms         UInt32,
    upstream_host      Nullable(String),
    error              Nullable(String),
    created_at         DateTime DEFAULT now()
) ENGINE = MergeTree()
ORDER BY (created_at, tenant_id, provider_id);
