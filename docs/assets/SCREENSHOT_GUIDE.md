# LoomGate Visual Assets & High-Conversion Screenshot Guide

First-time visitors decide whether to star a GitHub repo in **under 5 seconds**. Because LoomGate already has a polished dark-mode Tauri v2 Studio UI, capturing **3 crisp, real screenshots** (with realistic data and zero real API keys visible) will immediately differentiate LoomGate from generic CLI proxies.

Save all screenshots directly into `docs/assets/` using the exact filenames below so `README.md` and `README_ZH.md` render them automatically.

---

## Recommended Capture Setup (2 Minutes)

1. Start a clean mock demo environment so your real keys never appear on screen:
   ```bash
   # Terminal 1: Start 4-channel mock cluster (Anthropic x2, OpenAI, Gemini)
   python3 tests/mock_upstream.py

   # Terminal 2: Launch LoomGate Studio pointing to config.example.toml
   cd apps/desktop
   GATEWAY_CONFIG=../../config.example.toml GATEWAY_DB_PATH=../../.runtime/demo.db npm run tauri dev

   # Terminal 3: Seed realistic multi-turn cache hits, a Fork & Diff, and 1 Ctrl+C abort
   python3 tests/conformance_suite.py
   ```
   *(Or use your real `config.local.toml` for authentic coding answers — make sure **Redact Secrets & PII** is enabled and hide/mask all secret input fields.)*
2. Set the LoomGate window to **1280 × 800** (or **1440 × 900** at 100% / 200% DPI) for sharp text rendering on GitHub.
3. Prefer PNG format (`< 600 KB` each; optimize with TinyPNG/oxipng if larger).

---

## The 3 High-Conversion Screenshots to Capture

### 1. Hero Image — `docs/assets/loomgate-trajectory-diff.png` *(Highest Priority — Above the Fold in README)*
- **Where**: Open the **Trajectory Fork & Diff** tab.
- **What to show in frame**:
  1. Left sidebar showing at least 2–3 sessions with green `KV 90%` badges and `steps · forks` counts.
  2. Right panel showing a completed **Fork Diff** comparing two different models (e.g., `claude-sonnet-4-20250514` vs `gpt-4o` or `gemini-2.5-pro`) in **Line Diff** (red/green unified highlight) mode.
  3. Visible `TTFT Δ` and `Output Δ` pills + the **Redact Secrets & PII** checkbox + **Copy Markdown Report / Export SVG Card / Export PNG Card** buttons.
- **Why it converts**: Immediately proves in 3 seconds that LoomGate is NOT a dumb proxy — it is a local Trajectory Studio where you can branch and diff models side-by-side.

### 2. Main Control Studio — `docs/assets/loomgate-dashboard-channels.png`
- **Where**: Open the **Channels & Routing** tab.
- **What to show in frame**:
  1. Top bar with the glowing status dot, `LoomGate`, `127.0.0.1:8787`, and `Lock Mode` / `Stage-Aware` pill.
  2. **Explicit Routing Policy** card (`Lock Mode` vs `Stage-Aware`).
  3. **P2C Upstream Channel Pool** table displaying Anthropic, OpenAI, and Gemini channels with `healthy` badges (and optionally one `Offline Fallback` Ollama channel discovered via **Scan Local Models**), real latency probe feedback (`✓ 18ms · HTTP 200 OK`), and EWMA TTFT values.
- **Why it converts**: Shows multi-protocol P2C routing, explicit zero-silent-downgrade control, and zero-config GUI channel management in one glance.

### 3. Savings & Cache Hit Proof — `docs/assets/loomgate-savings-telemetry.png`
- **Where**: Open the **Savings & Telemetry** tab (select `30 Days` or `All Time`).
- **What to show in frame**:
  1. All 4 KPI cards populated with real numbers: **Total Requests & Sticky Hits**, **Prompt Cache Hit Rate %**, **Estimated Cache Savings ($)**, and **Ctrl+C Stop-Loss Aborts**.
  2. The pure-SVG **Model Distribution & Cache Hit Efficiency** horizontal bar chart + per-model breakdown table underneath.
- **Why it converts**: Gives concrete financial ROI ("this local gateway literally pays for itself via prompt cache affinity and Ctrl+C stop-loss").

---

## Pre-Commit Safety Checklist
- [ ] Verify NO real API keys (`sk-*`, `AIza*`, `ghp_*`) or private company code appear anywhere in the screenshots.
- [ ] Confirm all 3 files exist at:
  - `docs/assets/loomgate-trajectory-diff.png`
  - `docs/assets/loomgate-dashboard-channels.png`
  - `docs/assets/loomgate-savings-telemetry.png`
- [ ] Preview `README.md` on GitHub after pushing to verify above-the-fold framing.
