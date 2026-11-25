# loomgate

Official zero-dependency TypeScript SDK & CLI for **[LoomGate](https://github.com/yangdw2024/LoomGate)** — an Edge-Cloud Hybrid Microkernel AI Gateway written in Rust + Tauri v2.

## Install

```bash
npm install loomgate
# or run CLI directly
npx loomgate status
```

## SDK Quickstart

```ts
import { LoomGateClient } from "loomgate";

const gw = new LoomGateClient({
  baseUrl: "http://127.0.0.1:8787",
  token: process.env.LOOMGATE_TOKEN,
});

// 1. Check gateway health & P2C channel pool
console.log(await gw.health());
console.log(await gw.channels());

// 2. Call any upstream through Anthropic / OpenAI / Gemini native schemas
const res = await gw.generateContent(
  "gemini-2.5-pro",
  { contents: [{ role: "user", parts: [{ text: "Explain LoomGate IR." }] }] },
  { sessionId: "dev-session-1", routeMode: "lock" }
);
console.log(res.headers["x-gateway-actual-model"], res.data);

// 3. Fork & diff any local SQLite trajectory step on another model
const sessions = await gw.sessions();
const steps = await gw.sessionSteps(sessions[0].session_id);
const diff = await gw.forkStep(steps[0].step_id, "gpt-4o");
console.log("TTFT delta (ms):", diff.ttft_delta_ms);

// 4. Sign in to LoomGate Cloud Control Plane (hot-syncs team pool & sanitized telemetry)
await gw.cloudLogin("http://127.0.0.1:8790", "dev@loomgate.dev", "loomgate-dev-password");
```

## CLI Commands

```bash
loomgate status
loomgate sessions
loomgate steps <session_id>
loomgate fork <step_id> <target_model>
loomgate mcp list
loomgate mcp call mcp__filesystem__read_file '{"path":"Cargo.toml"}'
loomgate cloud login http://127.0.0.1:8790 dev@loomgate.dev loomgate-dev-password
```

## License

MIT — see [LoomGate Repository](https://github.com/yangdw2024/LoomGate).
