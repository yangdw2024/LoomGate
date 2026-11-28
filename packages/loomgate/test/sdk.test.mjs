import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { execFile } from "node:child_process";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { LoomGateClient } from "../dist/index.js";

const execFileAsync = promisify(execFile);

test("LoomGateClient interacts with health, channels, 3 protocols, fork & cloud endpoints", async () => {
  const seen = [];
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      seen.push({ method: req.method, url: req.url, auth: req.headers["authorization"] });
      res.setHeader("Content-Type", "application/json");
      if (req.url === "/healthz") {
        res.end(JSON.stringify({ status: "ok", route_mode: "lock" }));
      } else if (req.url === "/metrics/channels") {
        res.end(JSON.stringify({ channels: [{ id: "gemini-primary", protocol: "gemini", health: "healthy", inflight: 0, ewma_ttft_ms: null, total_requests: 1 }] }));
      } else if (req.url === "/api/cloud/status") {
        res.end(JSON.stringify({ connected: true, tenant_id: "team-default", synced_channels: 1, telemetry_uploaded: 2 }));
      } else if (req.url.startsWith("/api/analytics/savings")) {
        res.end(JSON.stringify({ window_days: 30, total_requests: 12, aborted_by_client_count: 2, total_cache_read_tokens: 450000, avg_cache_hit_ratio: 0.82, estimated_cache_savings_usd: 1.215, estimated_stop_loss_savings_usd: 0.024, by_model: [] }));
      } else if (req.url.startsWith("/api/local/discover")) {
        res.end(JSON.stringify({ providers: [{ kind: "ollama", display_name: "Ollama", suggested_channel_id: "local-ollama", base_url: "http://127.0.0.1:11434", port: 11434, models: ["qwen2.5-coder:7b"], latency_ms: 4 }] }));
      } else if (req.url === "/api/steps/export-diff") {
        res.end(JSON.stringify({ original_step_id: "s1", forked_step_id: "s2", original_model: "claude-sonnet-4-20250514", forked_model: "gpt-4o", redacted: true, redactions_applied: 2, sanitized_original_text: "[REDACTED_SECRET]", sanitized_forked_text: "ok", markdown: "# LoomGate Fork & Diff Report\n[REDACTED_SECRET]" }));
      } else if (req.url === "/v1beta/models/gemini-2.5-pro:generateContent") {
        res.setHeader("X-Gateway-Actual-Model", "gemini-2.5-pro");
        res.end(JSON.stringify({ candidates: [{ content: { parts: [{ text: "ok" }] } }] }));
      } else if (req.url === "/v1/responses" && req.method === "POST") {
        res.setHeader("X-Gateway-Actual-Model", "gpt-5-codex");
        res.end(
          JSON.stringify({
            id: "resp_test_1",
            object: "response",
            status: "completed",
            model: "gpt-5-codex",
            output: [{ type: "message", role: "assistant", content: [{ type: "output_text", text: "responses ok" }] }],
          })
        );
      } else if (req.url.startsWith("/api/inspector") && req.method === "GET") {
        res.end(
          JSON.stringify({
            entries: [
              {
                request_id: "req_insp_1",
                ts_ms: 1700000000000,
                downstream_protocol: "openai_responses",
                upstream_protocol: "openai_responses",
                requested_model: "gpt-5-codex",
                actual_model: "gpt-5-codex",
                channel_id: "openai-responses-primary",
                route_mode: "lock",
                sticky_hit: false,
                stream: false,
                ttft_ms: 12,
                status: "completed",
                input_tokens: 100,
                cache_read_tokens: 0,
                output_tokens: 20,
                guardrail_redactions: 1,
                blocked_reason: null,
                inbound_preview: "[REDACTED_SECRET]",
                canonical_ir_summary: "{}",
                upstream_preview: "responses ok",
                sse_events: [],
              },
            ],
          })
        );
      } else if (req.url === "/api/inspector" && req.method === "DELETE") {
        res.end(JSON.stringify({ ok: true }));
      } else {
        res.statusCode = 404;
        res.end(JSON.stringify({ error: "not found" }));
      }
    });
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  const client = new LoomGateClient({ baseUrl: `http://127.0.0.1:${port}`, token: "lg_test_token" });

  try {
    const h = await client.health();
    assert.equal(h.status, "ok");

    const chs = await client.channels();
    assert.equal(chs.length, 1);
    assert.equal(chs[0].id, "gemini-primary");

    const cloud = await client.cloudStatus();
    assert.equal(cloud.connected, true);

    const sav = await client.savings(30);
    assert.equal(sav.total_requests, 12);
    assert.equal(sav.aborted_by_client_count, 2);

    const locals = await client.discoverLocal([11434]);
    assert.equal(locals.length, 1);
    assert.equal(locals[0].kind, "ollama");

    const exp = await client.exportForkDiff("s1", "s2", true);
    assert.equal(exp.redacted, true);
    assert.equal(exp.redactions_applied, 2);

    const gem = await client.generateContent("gemini-2.5-pro", {
      contents: [{ role: "user", parts: [{ text: "hi" }] }],
    }, { sessionId: "s-1" });
    assert.equal(gem.status, 200);
    assert.equal(gem.headers["x-gateway-actual-model"], "gemini-2.5-pro");

    const resp = await client.responses(
      { model: "gpt-5-codex", input: "hello responses" },
      { sessionId: "s-resp-1" }
    );
    assert.equal(resp.status, 200);
    assert.equal(resp.headers["x-gateway-actual-model"], "gpt-5-codex");
    assert.equal(resp.data.object, "response");

    const entries = await client.inspector(10);
    assert.equal(entries.length, 1);
    assert.equal(entries[0].request_id, "req_insp_1");
    assert.equal(entries[0].inbound_preview, "[REDACTED_SECRET]");
    assert.ok(seen.some((s) => s.method === "GET" && s.url === "/api/inspector?limit=10"));

    const cleared = await client.clearInspector();
    assert.equal(cleared.ok, true);
    assert.ok(seen.some((s) => s.method === "DELETE" && s.url === "/api/inspector"));

    const cliPath = fileURLToPath(new URL("../bin/loomgate.mjs", import.meta.url));
    const { stdout } = await execFileAsync(process.execPath, [cliPath, "inspector", "5"], {
      env: { ...process.env, LOOMGATE_URL: `http://127.0.0.1:${port}`, LOOMGATE_TOKEN: "lg_test_token" },
    });
    const cliEntries = JSON.parse(stdout);
    assert.equal(cliEntries.length, 1);
    assert.equal(cliEntries[0].request_id, "req_insp_1");
    assert.ok(seen.some((s) => s.method === "GET" && s.url === "/api/inspector?limit=5"));

    assert.equal(seen[0].auth, "Bearer lg_test_token");
  } finally {
    server.close();
  }
});
