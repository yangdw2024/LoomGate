# LoomGate Post-v1.0 Roadmap

LoomGate `v1.0.0` established the local-first Rust microkernel (`gateway-core`), lossless 3-protocol Canonical IR (Anthropic / OpenAI / Gemini), Turn-1-stable Prefix-Cache Sticky + P2C routing, zero-buffer SSE `Drop` stop-loss, local SQLite WAL Trajectory Replay/Fork/Diff, and offline local-runtime fallback.

This roadmap outlines the post-`v1.0` milestones from `v1.1.0` through `v2.0.0`.

---

## `v1.1.0` — Protocol Breadth, Live Observability & Agent Ecosystem

Focus: expand protocol coverage for 2026 coding agents, add real-time traffic inspection and streaming safety enforcement, and modernize the desktop/web UI architecture.

- **OpenAI Responses API (`/v1/responses`)**: First-class ingress, streaming SSE event translation, and Canonical IR mapping alongside `/v1/messages` and `/v1/chat/completions`.
- **Live Traffic Inspector ("Charles for AI Agents")**: Real-time request/response stream viewer showing headers, routing decisions, cache hit attribution, token deltas, and raw vs. canonical payloads.
- **Streaming Guardrail Tool Interception**: Mid-stream inspection and blocking of high-risk tool calls before execution, complementing ingress PII/secret redaction.
- **`stdio` MCP Subprocess Bridge**: Native local subprocess lifecycle management to expose `stdio`-based MCP servers through the gateway middleware pipeline.
- **12 Built-in Agent Profiles**: Expand the Declarative Client Profile Registry from 8 to 12 profiles by adding **DeepSeek Harness**, **OpenCode**, **Claude Desktop**, and **Aider** (alongside existing atomic-takeover and wizard profiles).
- **Modular React Frontend**: Decompose the Studio console into focused, maintainable React feature modules with clean API boundaries.

---

## `v1.2.0` — Trajectory Studio 3.0, Benchmarking & Eval Pipelines

Focus: turn local SQLite WAL trajectories into an interactive debugging, benchmarking, and evaluation workbench.

- **Trajectory Studio 3.0 (Edit & Replay Any Turn)**: Edit system prompts, tool outputs, or user/assistant messages at any historical turn and replay downstream branches against any configured model or channel.
- **Built-in `loomgate bench`**: Reproducible A/B benchmark CLI comparing round-robin routing vs. LoomGate Prefix-Cache Sticky routing (cache read ratio, TTFT, cost delta, and latency percentiles).
- **Guardrail-Redacted JSONL Eval Dataset Export**: One-click export of scrubbed multi-turn trajectories to standard JSONL evaluation datasets with secrets and PII deterministically stripped.

---

## `v1.3.0` — Production Cloud Control Plane & Team Governance

Focus: graduate the optional privacy-preserving Cloud Control Plane (hashed IDs & numeric metrics only — raw prompts remain local) to production-grade multi-node storage and team controls.

- **Production Storage Backends**:
  - **PostgreSQL (`sqlx`)** for durable tenant, key, policy, and routing state.
  - **Redis (`redis-rs` with Lua `EVALSHA`)** for atomic distributed rate limiting and quota accounting.
  - **ClickHouse (async batching)** for high-throughput telemetry ingestion and historical savings analytics.
- **Team Budget Circuit Breakers**: Configurable per-team, per-project, and per-key spend thresholds with soft warnings, automatic model-tier fallback, or hard stop-loss circuit breaking.

---

## `v2.0.0` — Native Harness Integration, Mobile Cockpit & Universal Distribution

Focus: seamless integration into flagship coding-agent ecosystems, remote operational visibility, and zero-friction installation across every OS.

- **Official DeepSeek Harness Plugin (`dsh-loomgate`)**: Deep integration exposing LoomGate channel health, sticky-cache savings, live traffic inspection, and one-click trajectory replay directly inside DeepSeek Harness.
- **Mobile / Tailscale Read-Only Cockpit & Emergency Kill-Switch**: Lightweight mobile-friendly status view over Tailscale for monitoring live spend, cache hit rates, and active streams, plus an authenticated emergency kill-switch to halt runaway loops remotely.
- **Native Package Managers**: Official distribution via **Homebrew** (macOS/Linux), **WinGet**, and **Scoop** (Windows) alongside Tauri installers, Docker Compose, and npm (`loomgate`).

---

## Milestone Summary

| Version | Theme | Key Deliverables |
|---|---|---|
| **`v1.1.0`** | Protocol & Observability | `/v1/responses`, Live Traffic Inspector, Streaming Guardrail tool interception, `stdio` MCP bridge, 12 built-in agent profiles, modular React frontend |
| **`v1.2.0`** | Studio 3.0 & Evals | Edit & Replay any turn, `loomgate bench` (round-robin vs. sticky A/B), Guardrail-redacted JSONL Eval export |
| **`v1.3.0`** | Production Control Plane | `sqlx` PostgreSQL, `redis-rs` Lua `EVALSHA`, ClickHouse async batching, team budget circuit breakers |
| **`v2.0.0`** | Ecosystem & Distribution | `dsh-loomgate` plugin, Mobile/Tailscale cockpit & kill-switch, Homebrew / WinGet / Scoop packages |
