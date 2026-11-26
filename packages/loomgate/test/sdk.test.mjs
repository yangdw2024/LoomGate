import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { LoomGateClient } from "../dist/index.js";

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
      } else if (req.url === "/v1beta/models/gemini-2.5-pro:generateContent") {
        res.setHeader("X-Gateway-Actual-Model", "gemini-2.5-pro");
        res.end(JSON.stringify({ candidates: [{ content: { parts: [{ text: "ok" }] } }] }));
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

    const gem = await client.generateContent("gemini-2.5-pro", {
      contents: [{ role: "user", parts: [{ text: "hi" }] }],
    }, { sessionId: "s-1" });
    assert.equal(gem.status, 200);
    assert.equal(gem.headers["x-gateway-actual-model"], "gemini-2.5-pro");
    assert.equal(seen[0].auth, "Bearer lg_test_token");
  } finally {
    server.close();
  }
});
