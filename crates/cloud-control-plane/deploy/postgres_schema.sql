-- LoomGate Stage 3 Cloud Control Plane — PostgreSQL Core Schema
-- Stores tenants, users, team channel pools, routing/guardrail rules, and double-entry billing ledger.

CREATE TABLE IF NOT EXISTS tenants (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    plan            TEXT NOT NULL DEFAULT 'team',
    balance_micros  BIGINT NOT NULL DEFAULT 0 CHECK (balance_micros >= 0),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    email           TEXT NOT NULL UNIQUE,
    password_hash   TEXT NOT NULL,
    role            TEXT NOT NULL DEFAULT 'member',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS team_channels (
    id              TEXT NOT NULL,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    protocol        TEXT NOT NULL CHECK (protocol IN ('anthropic', 'openai', 'gemini')),
    base_url        TEXT NOT NULL,
    encrypted_key   TEXT NOT NULL,
    weight          INTEGER NOT NULL DEFAULT 100 CHECK (weight > 0),
    models_json     JSONB NOT NULL DEFAULT '[]'::jsonb,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant_id           TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    default_route_mode  TEXT NOT NULL DEFAULT 'lock',
    rpm_limit           INTEGER NOT NULL DEFAULT 120,
    redact_secrets      BOOLEAN NOT NULL DEFAULT TRUE,
    redact_pii          BOOLEAN NOT NULL DEFAULT TRUE,
    blocked_tools_json  JSONB NOT NULL DEFAULT '["drop_database"]'::jsonb,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS billing_ledger (
    reservation_id  TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    model           TEXT NOT NULL,
    reserved_micros BIGINT NOT NULL,
    settled_micros  BIGINT,
    status          TEXT NOT NULL CHECK (status IN ('reserved', 'settled', 'released')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    settled_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_billing_ledger_tenant_created
    ON billing_ledger (tenant_id, created_at DESC);
