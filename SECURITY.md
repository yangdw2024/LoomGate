# Security & Privacy Policy

## Architectural Privacy Boundaries

LoomGate enforces a strict **Local-First Privacy** architecture:

1. **Local SQLite WAL Only for Raw Content**:
   - Complete prompts, system instructions, tool arguments, and model responses are persisted **exclusively** in your local SQLite WAL file (`GATEWAY_DB_PATH`).
   - When connected to the optional Cloud Control Plane (`crates/cloud-control-plane`), the edge node uploads **only** a strict desensitized struct (`SanitizedTelemetryUpload`) containing a SHA-256 hash of the session ID and numeric/categorical metrics (`ttft_ms`, token counts, model, status).
   - As defense-in-depth, `cloud-control-plane/src/telemetry.rs` actively rejects (`HTTP 400`) any telemetry payload containing forbidden raw keys (`prompt`, `messages`, `system`, `response_text`, `content`, `request_json`, `tool_calls_json`).

2. **Split Authentication (`api_key` vs `admin_password`)**:
   - Inference endpoints (`/v1/*`, `/v1beta/*`) are guarded by `api_key` (`x-api-key`, `Authorization: Bearer`, or `x-goog-api-key`).
   - Control & management endpoints (`/api/*`, `/__admin/*`) are guarded by `admin_password` (`x-admin-password` or `Authorization: Bearer`).
   - In Docker / Browser Web Console mode, the admin password is stored **only in ephemeral browser `sessionStorage`** (never in `localStorage`, cookies, or bundled JavaScript).
   - When both values are unset/empty, LoomGate runs in passwordless localhost dev mode. If you bind LoomGate to `0.0.0.0` (e.g., team Docker deployment), **always configure both `api_key` and `admin_password`**.

3. **Outbound & Export Guardrail Redaction**:
   - The `GuardrailMiddleware` can redact secrets (`sk-ant-*`, `sk-*`, `Bearer <token>`, `AIza*`, `ghp_*`, `gho_*`, `AKIA*`) and PII (emails, phone numbers) before requests leave your host, and block high-risk upstream tool calls.
   - Trajectory Studio 2.0 Markdown reports and SVG/PNG share cards enable `redact_secrets = true` by default so accidental secret leaks are scrubbed prior to sharing.

4. **Credential Hygiene**:
   - `config.local.toml` is listed in `.gitignore` and `.dockerignore` and must never be committed to source control.
   - Atomic file takeover creates `.edge-gateway.bak` snapshots locally and uses sibling temp-file renames to avoid partial writes.

## Reporting a Vulnerability

If you discover a security vulnerability in LoomGate:
- **Do not** open a public GitHub Issue containing exploit details or credentials.
- Use **[GitHub Private Vulnerability Reporting](https://github.com/yangdw2024/LoomGate/security/advisories/new)** on `yangdw2024/LoomGate`.
- Include a clear description, affected version, reproduction steps, and suggested mitigation if available. We aim to acknowledge reports within 48 hours and release patched builds promptly.
