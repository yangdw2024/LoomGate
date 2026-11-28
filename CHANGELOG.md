# Changelog

All notable changes to **LoomGate** are documented in this file.

## [1.1.0] - 2026-11-28

### Added
- **Native OpenAI Responses API (`POST /v1/responses`)**:
  - Added full lossless bidirectional `OpenAiResponsesCodec` (`crates/gateway-core/src/codec/openai_responses.rs`) supporting `instructions`, multi-item `input` (`message`, `function_call`, `function_call_output`, `reasoning`), `strict` tool schemas, `reasoning.effort`, cached token usage (`input_tokens_details.cached_tokens`), and native Responses SSE events (`response.created`, `response.output_text.delta`, `response.reasoning_summary_text.delta`, `response.function_call_arguments.delta`, `response.completed`).
  - Seamless cross-protocol translation between `/v1/responses`, Anthropic `/v1/messages`, OpenAI `/v1/chat/completions`, and Gemini native endpoints.
- **Live Traffic Inspector ("Charles for AI Agents")**:
  - Added bounded 200-entry `LiveTrafficInspector` ring buffer (`crates/gateway-core/src/inspector.rs`) with automatic Guardrail secret/PII redaction and UTF-8 safe preview truncation.
  - Exposed `GET/DELETE /api/inspector`, `GET /api/inspector/:request_id`, Tauri IPC commands, `loomgate inspector` CLI / SDK methods, and a brand-new **Live Inspector** split-pane UI tab displaying Canonical IR summaries, redacted Inbound/Upstream wire payloads, blocked-reason alerts, and ordered SSE event timelines.
- **Streaming Guardrail Tool Interception**:
  - Added `StreamGuardrailChecker` (`crates/gateway-core/src/middleware/guardrail.rs`) inside the live SSE pump to intercept blocked tool names and split dangerous argument chunks mid-stream, immediately canceling upstream TCP and recording `guardrail_blocked`.
  - Fixed UTF-8 multi-byte boundary safety in `redact_bearer_tokens`.
- **`stdio` MCP Subprocess Bridge**:
  - Extended `McpBridgeMiddleware` (`crates/gateway-core/src/middleware/mcp_bridge.rs`) with backward-compatible `McpTransport::Stdio` JSON-RPC 2.0 discovery (`tools/list`) and execution (`tools/call`) over `tokio::process::Command`.
- **12 Built-in Agent Profiles & Modular React Architecture**:
  - Added **DeepSeek Harness**, **OpenCode**, **Claude Desktop**, and **Aider** to `crates/gateway-core/src/cli_takeover.rs` (12 built-in profiles total) and documented `docs/ROADMAP.md`.
  - Refactored monolithic `apps/desktop/src/App.tsx` (1,804 → 308 lines) into clean page modules under `apps/desktop/src/pages/` and `apps/desktop/src/utils/diff.ts`.
- Expanded Rust workspace unit tests to **36/36** and end-to-end conformance suite to **22/22**.

## [1.0.0] - 2026-11-26

### Highlights — General Availability Release
**LoomGate v1.0.0** is the first stable, general-availability release of the Edge-Cloud Hybrid Microkernel AI Gateway — unifying native desktop ergonomics, one-command Docker team hosting, lossless 3-protocol translation, local offline resilience, and privacy-first trajectory observability.

### Added
- **Unified Dual-Runtime React Console (`apps/desktop/src/api/client.ts`)**:
  - Single React/Vite codebase runs seamlessly as both a **Tauri v2 Desktop Studio** (native IPC `invoke`) and a **Browser Web Console** (same-origin HTTP REST API).
  - Automatic ephemeral `sessionStorage` Admin Password unlock bar (`x-admin-password`) when `admin_password` is enabled (never persisted to `localStorage` or bundled source); passwordless on localhost when unset.
- **Built-in Static Web Hosting & Multi-Stage Docker Deployment**:
  - `gateway-daemon` supports `--web-dist <dir>` / `LOOMGATE_WEB_DIST` with path-traversal protection and strict API precedence (`/v1/*`, `/v1beta/*`, `/api/*`, `/__admin/*` never masked by SPA HTML), plus REST parity endpoints (`/api/status`, `/api/route-mode`, `/api/clients/:id/takeover`, `/api/clients/:id/restore`).
  - Added multi-stage root `Dockerfile`, `.dockerignore`, and `deploy/docker-compose.yml` (`docker compose -f deploy/docker-compose.yml up -d` serves both the 3-protocol AI Gateway and Web Control Studio at `http://localhost:8787`).
- **Open-Source Bilingual Storefront & Ecosystem Governance**:
  - Flagship English `README.md` and Chinese `README_ZH.md` with Mermaid architecture diagram, proxy comparison matrix, and 30-second quickstart (Desktop / Docker / npm CLI).
  - Added `docs/EXTENSIONS.md` (custom Client Profile TOML/JSON, custom Onion `Middleware` Trait, custom `ProtocolCodec` Trait), `CONTRIBUTING.md`, and `SECURITY.md`.
- Expanded end-to-end conformance suite to **18/18** and Rust workspace unit tests to **26/26** across all 4 crates.

## [0.5.0] - 2026-11-25

### Added
- **Local Model Auto-Discovery & Offline Fallback (`crates/gateway-core/src/local_discovery.rs`)**:
  - Parallel non-blocking scanner (`<= 350ms` per-probe timeout) detecting Ollama (`127.0.0.1:11434/api/tags`), LM Studio (`127.0.0.1:1234/v1/models`), vLLM (`127.0.0.1:8000/v1/models`), and user-specified custom localhost ports.
  - One-click import (`POST /api/local/import` & Desktop **Scan Local Models** button) adding discovered local services as OpenAI-compatible channels with optional `offline_fallback = true`, automatically taking over with `X-Gateway-Offline-Fallback: true` when all cloud channels trip or go offline.
- **Local Savings & Stop-Loss Telemetry Dashboard (`trajectory.rs` + `/api/analytics/savings`)**:
  - Aggregates local SQLite WAL trajectories over `7d` / `30d` / `All Time`: total requests, sticky hits, Prompt Caching read tokens & hit rate, `aborted_by_client` Ctrl+C stop-loss count, and estimated USD savings using built-in + `[model_pricing]`-overridable pricing.
  - Added lightweight zero-dependency CSS/SVG **Savings & Telemetry** tab in LoomGate Studio and `loomgate savings [days]` / `loomgate local scan` CLI & SDK methods.
- **Trajectory Studio 2.0 Export & Share (`export_fork_diff_markdown` + Pure Canvas 2D PNG / Pure SVG)**:
  - Added `POST /api/steps/export-diff`, Tauri `export_fork_diff_markdown`, and `loomgate diff-export`: reuses `GuardrailMiddleware` redaction (including `sk-*`, `Bearer <token>`, `AIza*`, `ghp_*`, `AKIA*`, emails, and CN phone numbers — enabled by default) to generate Markdown comparison reports.
  - Added **Copy Markdown Report**, **Export SVG Card** (pure SVG, zero `<foreignObject>`), and **Export PNG Card** (pure Canvas 2D API, zero WebView2 canvas taint) in Desktop Studio.
- Expanded `tests/conformance_suite.py` to **17/17** and Rust workspace unit tests to **25/25**.

## [0.4.0] - 2026-11-25

### Added
- **Declarative Client Profile Registry (`crates/gateway-core/src/cli_takeover.rs`)**:
  - Generalized CLI takeover into a declarative profile registry with 3 built-in Atomic Takeover profiles (`Claude Code`, `Codex CLI`, `Gemini CLI`) and 5 built-in Setup Wizard profiles (`Cursor`, `Cline / Roo Code`, `Cherry Studio`, `Trae`, `Generic OpenAI / Anthropic / Gemini SDK`).
  - Supports loading user-defined custom client profiles from `~/.loomgate/client_profiles.json`, `~/.loomgate/client_profiles.toml`, or `LOOMGATE_CLIENT_PROFILES` without modifying Rust code.
  - Exposed `GET /api/clients` on `gateway-daemon` and upgraded Desktop **Client Integrations** tab with one-click Copy Env / Copy JSON wizards.
- **Pure GUI Channel Pool Management & Upstream Network Generalization**:
  - Added `enabled`, `proxy_url` (per-channel HTTP/HTTPS/SOCKS5 via `reqwest` `socks`), `fallback_models` (explicit Stage-Aware fallback chain), and `quota_endpoint` to `ChannelConfig`.
  - Added `normalize_base_url()` to automatically strip redundant trailing `/v1` and `/v1beta` segments and prevent double-prefixing.
  - Added runtime channel CRUD (`upsert_channel`, `delete_channel`, `set_channel_enabled`) with atomic `config.local.toml` persistence (`GatewayConfig::save_atomic`).
  - Added `test_channel_connection` latency + status + optional quota probe API and full visual CRUD/probe UI with live 429 cooldown & quarantine countdowns in Tauri Desktop.
- **Claude CLI Preflight & Split Authentication**:
  - Added `POST /v1/messages/count_tokens` to both `gateway-daemon` and Tauri embedded proxy (forwards to native Anthropic upstreams when available; falls back seamlessly to deterministic Canonical IR estimation for OpenAI/Gemini upstreams).
  - Split authentication into `api_key` (`/v1/*`, `/v1beta/*`) and `admin_password` (`/api/*`, `/__admin/*`) while retaining passwordless localhost dev mode when unset.
- **GitHub Actions CI & Cross-Platform Release Automation**:
  - Added `.github/workflows/ci.yml` (Rust clippy, workspace tests, 14/14 conformance suite, desktop frontend build, npm SDK tests).
  - Added `.github/workflows/release.yml` using `tauri-apps/tauri-action` to build and publish Windows (NSIS/MSI), macOS (DMG), and Linux (AppImage/deb) installers automatically on `v*` tags.
- Expanded `tests/conformance_suite.py` to **14/14** tests and workspace Rust unit tests to **22/22**.

## [0.3.0] - 2026-11-25

### Added
- **Native Gemini Protocol Codec (`crates/gateway-core/src/codec/gemini.rs`)**: Full `generateContent` & `streamGenerateContent?alt=sse`, Tool Calling, `thought_signature` sidecar caching & cross-turn hydration, and Google API error formatting.
- **Onion-Model Middleware Pipeline (`crates/gateway-core/src/middleware/`)**: Symmetric pre/post pipeline with Ingress RPM/Burst rate limiting, Guardrail secret/PII redaction & high-risk tool interception, Context/Memory workspace injection, and unified MCP Server bridge (`mcp__<server>__<tool>`).
- **Cloud Control Plane (`crates/cloud-control-plane`) & Edge-Cloud Sync (`crates/gateway-core/src/cloud_sync.rs`)**: Standalone Axum control plane, PostgreSQL/ClickHouse schemas, Redis Lua two-phase atomic reserve/settle billing script, hot team-pool merging, and strictly desensitized telemetry analytics.
- **LoomGate Desktop Branding & UX**: Formal icons, dynamic Lock (Blue) / Stage-Aware (Green) tray LED, `—` cold-start TTFT display, Windows autostart, local auth token UI, and line-level LCS Fork & Diff highlight view.
- **Official npm Package (`packages/loomgate`)**: Published zero-dependency TypeScript SDK & CLI (`loomgate`).

## [0.2.1] - 2026-11-25

### Added
- Initial verified baseline: Lossless Canonical IR for Anthropic & OpenAI, UTF-8 safe zero-buffer SSE FSM with Drop cancel propagation, Turn-1-stable Prefix-Cache Sticky + P2C Router, 4-Tier Error Governance, SQLite WAL Trajectory Store with Fork & Diff, Atomic CLI Takeover, and Tauri v2 Desktop Studio with graceful shutdown.
