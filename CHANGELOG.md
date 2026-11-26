# Changelog

All notable changes to **LoomGate** are documented in this file.

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
