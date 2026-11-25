# LoomGate

**Edge-Cloud Hybrid Microkernel AI Gateway** (Rust Workspace + Tauri v2 Desktop + Onion Middleware + Cloud Control Plane + TypeScript SDK/CLI).

Built for local-first AI coding agents (Claude Code, Codex CLI, Gemini CLI) and team-scale AI engineering with **zero silent downgrade**, **lossless 3-protocol Canonical IR**, and **strict local privacy**.

## Architecture Overview

```text
  Claude Code / Codex / Gemini CLI / SDKs
                     │
       ┌─────────────▼─────────────┐
       │  LoomGate Local Data Plane │  (gateway-daemon / Tauri v2 embedded on 127.0.0.1:8787)
       │  • /v1/messages            │  Anthropic Messages API
       │  • /v1/chat/completions    │  OpenAI Chat Completions API
       │  • /v1beta/models/:action  │  Gemini Native (generateContent / streamGenerateContent?alt=sse)
       └─────────────┬─────────────┘
                     │
       ┌─────────────▼───────────────────────────────────────────────┐
       │  crates/gateway-core (Embeddable Microkernel)               │
       │  1. Onion Middleware Pipeline:                              │
       │     Ingress RateLimit → Guardrail (PII/Secret & Tool Block) │
       │     → Context/Memory Injection → MCP Server Bridge          │
       │  2. Lossless Canonical IR + Sidecar SignatureCache          │
       │     (cache_control, thinking/signature, thought_signature,  │
       │      strict Tool Schema, UTF-8 Zero-Buffer SSE FSM + Drop)  │
       │  3. Prefix-Cache Sticky + P2C Router (Lock / Stage-Aware)   │
       │  4. 4-Tier Error Governance (L1 / L2 / L3 / L4)             │
       │  5. Local SQLite WAL Append-Only Trajectory + Fork & Diff   │
       └──────┬──────────────────────────────────┬───────────────────┘
              │ (Raw prompts stay LOCAL)          │ (Sanitized metadata ONLY)
              ▼                                   ▼
     Upstream LLM Channels             crates/cloud-control-plane (:8790)
     (Anthropic / OpenAI / Gemini)     • Team Channel Pool Hot-Sync (PostgreSQL schema)
                                       • Atomic 2-Phase Reserve/Settle Billing (Redis Lua)
                                       • Desensitized Telemetry Analytics (ClickHouse)
```

## Workspace Layout

| Crate / Package | Description |
|---|---|
| `crates/gateway-core` | Pure embeddable microkernel: Anthropic/OpenAI/Gemini codecs, UTF-8 zero-buffer SSE FSM, Sticky P2C router, 4-tier error governance, SQLite WAL trajectory + Fork & Diff, atomic CLI takeover, onion `Middleware` pipeline, and `CloudSyncClient`. |
| `crates/gateway-daemon` | Standalone local HTTP daemon (`127.0.0.1:8787`) exposing inference, trajectory, middleware/MCP, and cloud sync endpoints. |
| `crates/cloud-control-plane` | Standalone Cloud Control Plane (`127.0.0.1:8790`) + production `deploy/` manifests (`postgres_schema.sql`, `redis_billing.lua`, `clickhouse_schema.sql`, `docker-compose.yml`). |
| `apps/desktop` | Tauri v2 Desktop Control Studio embedding `gateway-core` in-process, with dynamic system tray status LED (Lock Blue / Stage-Aware Green), autostart, local auth token UI, CLI atomic takeover, Cloud Team sync, and line-level Fork & Diff view. |
| `packages/loomgate` | Zero-dependency TypeScript SDK & CLI published on npm as [`loomgate`](https://www.npmjs.com/package/loomgate). |

## Quickstart

### 1. Build & Test (Rust Workspace)

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### 2. Run Full 11-Test Conformance Suite

```bash
# Terminal 1: Mock 4-channel cluster (Anthropic x2, OpenAI, Gemini)
python tests/mock_upstream.py
# Terminal 2: Cloud Control Plane
cargo run -p cloud-control-plane
# Terminal 3: Edge Daemon
GATEWAY_CONFIG=config.example.toml cargo run -p gateway-daemon
# Terminal 4: 11/11 Conformance Suite
python tests/conformance_suite.py
```

### 3. Launch Tauri Desktop Studio

```bash
cd apps/desktop
npm install
GATEWAY_CONFIG=../../config.example.toml npm run tauri dev
```

### 4. Use the Official npm SDK & CLI

```bash
npm install loomgate
# or
npx loomgate status
npx loomgate sessions
npx loomgate fork <step_id> gpt-4o
npx loomgate cloud login http://127.0.0.1:8790 dev@loomgate.dev loomgate-dev-password
```

## Security & Privacy Guarantees

1. **Lossless Fidelity**: Every request, response, SSE frame, and error traverses Canonical IR — never silently dropping `cache_control`, `thinking/signature`, `thought_signature`, or `strict` tool schemas.
2. **Explicit Transparent Routing**: `Lock` mode never silently downgrades; every response emits `X-Gateway-Actual-Model`, `X-Gateway-Channel-Id`, `X-Gateway-Route-Mode`, and `X-Gateway-Sticky-Hit`.
3. **Local-First Privacy**: Full prompts and response bodies remain exclusively in local SQLite WAL. Cloud telemetry uses a strict schema containing only SHA-256 hashed session IDs and numeric/categorical metrics — and the Cloud Control Plane actively rejects any payload containing raw prompt/response fields.

## License

MIT
