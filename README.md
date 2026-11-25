# LoomGate

**Edge-Cloud Hybrid Microkernel AI Gateway** (Rust Workspace + Tauri v2 Desktop + Onion Middleware + Cloud Control Plane + Official TypeScript SDK/CLI).

Built for zero-friction AI coding agents (**Claude Code, Codex CLI, Gemini CLI, Cursor, Cline / Roo Code, Cherry Studio, Trae, and any OpenAI/Anthropic/Gemini SDK**) with **zero silent downgrade**, **lossless 3-protocol Canonical IR**, **pure GUI channel pool management**, and **strict local-first privacy**.

---

## Download & Install (v0.4.0 Zero-Config Out-of-the-Box)

### Desktop App (Recommended)
Download the pre-built native installer from **[GitHub Releases](https://github.com/yangdw2024/LoomGate/releases)**:
- **Windows**: `LoomGate_0.4.0_x64-setup.exe` (NSIS) or `.msi`
- **macOS**: `LoomGate_0.4.0_aarch64.dmg`
- **Linux**: `LoomGate_0.4.0_amd64.AppImage` or `.deb`

Launch **LoomGate Studio**, click **+ Add Channel** in the **Channels & Routing** tab (trailing `/v1` or `/v1beta` is auto-stripped; optionally configure per-channel HTTP/SOCKS5 proxy, Stage-Aware fallback models, and quota probe URL), click **Test** to verify latency, then head to **Client Integrations** to connect your favorite coding agent in one click.

### Official npm SDK & CLI
```bash
npm install loomgate
# or invoke CLI directly
npx loomgate status
npx loomgate sessions
npx loomgate fork <step_id> gpt-4o
npx loomgate cloud login http://127.0.0.1:8790 dev@loomgate.dev loomgate-dev-password
```

---

## All-Client Integrations (Declarative Client Profile Registry)

LoomGate v0.4.0 replaces hardcoded CLI adapters with a **Declarative Client Profile Registry** (`GET /api/clients` & Tauri **Client Integrations** tab):

| Client | Integration Mode | Highlights |
|---|---|---|
| **Claude Code** | Atomic Config Takeover (`.bak` rollback) | Patches `~/.claude/settings.json` atomically; full `/v1/messages/count_tokens` preflight support even on OpenAI/Gemini upstreams. |
| **Codex CLI** | Atomic Config Takeover (`.bak` rollback) | Patches `~/.codex/config.toml` atomically with lossless one-click restore. |
| **Gemini CLI** | Atomic Config Takeover (`.bak` rollback) | Patches `~/.gemini/settings.json` atomically with native `thought_signature` sidecar hydration. |
| **Cursor** | Interactive Setup Wizard | One-click copy `OPENAI_BASE_URL` & JSON config. |
| **Cline / Roo Code** | Interactive Setup Wizard | Anthropic & OpenAI-compatible presets with `cache_control` preservation. |
| **Cherry Studio** | Interactive Setup Wizard | Unified multi-protocol provider snippet. |
| **Trae** | Interactive Setup Wizard | OpenAI-compatible custom model endpoint preset. |
| **Generic SDK** | Interactive Setup Wizard | Drop-in env vars for Python / TypeScript / Go / Rust SDKs. |

### Adding Custom Client Profiles (Zero Rust Code Required)
Drop a JSON or TOML file at `~/.loomgate/client_profiles.toml` (or `~/.loomgate/client_profiles.json`, or set `LOOMGATE_CLIENT_PROFILES`):

```toml
[[profiles]]
id = "aider"
display_name = "Aider"
category = "CLI Agent"
kind = "atomic_takeover"
description = "Custom Aider profile"

[profiles.atomic]
relative_config_path = ".aider/config.json"
format = "json"

[profiles.atomic.set_fields]
"openai.api_base" = "{{GATEWAY_URL}}/v1"
```

---

## Architecture Overview

```text
  Claude Code / Codex / Gemini CLI / Cursor / Cline / Cherry Studio / Trae / SDKs
                                       │
         ┌─────────────────────────────▼─────────────────────────────┐
         │  LoomGate Local Data Plane (127.0.0.1:8787)               │
         │  • /v1/messages & /v1/messages/count_tokens (Anthropic)   │
         │  • /v1/chat/completions                     (OpenAI)      │
         │  • /v1beta/models/:action                   (Gemini)      │
         │  • Split Auth: api_key (/v1*, /v1beta*) | admin_password (/api*) │
         └─────────────────────────────┬─────────────────────────────┘
                                       │
         ┌─────────────────────────────▼─────────────────────────────┐
         │  crates/gateway-core (Embeddable Microkernel)             │
         │  1. Onion Middleware Pipeline:                            │
         │     Ingress RateLimit → Guardrail (PII/Secret & Tool Block) │
         │     → Context/Memory Injection → MCP Server Bridge        │
         │  2. Lossless Canonical IR + Sidecar SignatureCache        │
         │  3. Prefix-Cache Sticky + P2C Router (Lock / Stage-Aware) │
         │     + Per-Channel HTTP/SOCKS5 Proxy + Live 429 Cooldown   │
         │  4. 4-Tier Error Governance (L1 / L2 / L3 / L4)           │
         │  5. Local SQLite WAL Trajectory Store + Line-Level Fork/Diff │
         └──────────────┬────────────────────────────────┬───────────┘
                        │ (Raw prompts stay LOCAL)        │ (Sanitized metadata ONLY)
                        ▼                                 ▼
       Upstream LLM Channels                     crates/cloud-control-plane (:8790)
       (Anthropic / OpenAI / Gemini)             • PostgreSQL Team Pool Hot-Sync
                                                 • Redis Lua Atomic 2PC Reserve/Settle
                                                 • ClickHouse Desensitized Analytics
```

---

## Workspace Layout

| Crate / Package | Description |
|---|---|
| `crates/gateway-core` | Pure embeddable microkernel: Anthropic/OpenAI/Gemini codecs, UTF-8 zero-buffer SSE FSM, Sticky P2C router with live cooldown countdown & per-channel proxy, 4-tier error governance, SQLite WAL trajectory + Fork & Diff, Declarative Client Profile Registry, Onion `Middleware` pipeline, atomic `config.local.toml` writer, and `CloudSyncClient`. |
| `crates/gateway-daemon` | Standalone local HTTP daemon (`127.0.0.1:8787`) with `/v1/messages/count_tokens`, split `api_key` / `admin_password` auth, live channel CRUD/probe, trajectory, middleware/MCP, and cloud sync APIs. |
| `crates/cloud-control-plane` | Standalone Cloud Control Plane (`127.0.0.1:8790`) + production `deploy/` manifests (`postgres_schema.sql`, `redis_billing.lua`, `clickhouse_schema.sql`, `docker-compose.yml`). |
| `apps/desktop` | Tauri v2 Desktop Control Studio embedding `gateway-core` in-process, with dynamic tray status LED (Lock Blue / Stage-Aware Green), GUI channel pool CRUD & probe, Client Integrations Hub, Split Security UI, Windows autostart, line-level Fork & Diff, and Cloud Team panel. |
| `packages/loomgate` | Zero-dependency TypeScript SDK & CLI published on npm as [`loomgate`](https://www.npmjs.com/package/loomgate). |

---

## Building from Source & Verification

```bash
# 1. Lint & Unit Tests (22 tests across workspace)
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Build Desktop Frontend & Test npm SDK
cd apps/desktop && npm ci && npm run build && cd ../..
cd packages/loomgate && npm test && cd ../..

# 3. Run Full 14/14 End-to-End Conformance Suite
python tests/mock_upstream.py &
cargo run -p cloud-control-plane &
GATEWAY_CONFIG=config.example.toml cargo run -p gateway-daemon &
python tests/conformance_suite.py
```

---

## Security & Privacy Invariants

1. **Lossless Fidelity**: Every request, response, SSE frame, and error traverses Canonical IR — never silently dropping `cache_control`, `thinking/signature`, `thought_signature`, or `strict` tool schemas.
2. **Explicit Transparent Routing**: `Lock` mode never silently downgrades; `fallback_models` only activates in `Stage-Aware` mode; every response emits `X-Gateway-Actual-Model`, `X-Gateway-Channel-Id`, `X-Gateway-Route-Mode`, and `X-Gateway-Sticky-Hit`.
3. **Split Local Auth**: Separate `api_key` (inference endpoints) and `admin_password` (control/config endpoints), while remaining passwordless on localhost when unset.
4. **Local-First Privacy**: Full prompts and response bodies remain exclusively in local SQLite WAL. Cloud telemetry transmits only SHA-256 hashed session IDs and numeric/categorical metrics.

## License

MIT
