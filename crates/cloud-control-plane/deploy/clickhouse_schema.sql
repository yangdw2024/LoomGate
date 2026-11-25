-- LoomGate Stage 3 Cloud Control Plane — ClickHouse Desensitized Trajectory Analytics Schema
-- PRIVACY INVARIANT: Raw prompts, system instructions, tool arguments, and response bodies
-- MUST NEVER be stored in ClickHouse. Only hashed IDs & numeric/categorical telemetry are persisted.

CREATE TABLE IF NOT EXISTS sanitized_trajectories (
    ts                  DateTime64(3, 'UTC') DEFAULT now64(3),
    tenant_id           LowCardinality(String),
    edge_node_id        LowCardinality(String),
    session_hash        FixedString(64),
    step_id             String,
    protocol            LowCardinality(String),
    channel_id          LowCardinality(String),
    actual_model        LowCardinality(String),
    route_mode          LowCardinality(String),
    sticky_hit          UInt8,
    ttft_ms             UInt32,
    input_tokens        UInt32,
    cache_read_tokens   UInt32,
    output_tokens       UInt32,
    cost_micros         UInt64,
    status              LowCardinality(String)
)
ENGINE = MergeTree()
PARTITION BY toYYYYMM(ts)
ORDER BY (tenant_id, actual_model, ts, step_id)
TTL toDateTime(ts) + INTERVAL 180 DAY;
