#!/usr/bin/env node
import { LoomGateClient } from "../dist/index.js";

function usage() {
  console.log(`LoomGate CLI v0.4.0 — Edge-Cloud Hybrid Microkernel AI Gateway

Usage:
  loomgate status                                Show health, channels, middleware & cloud status
  loomgate sessions                              List local SQLite trajectory sessions
  loomgate steps <session_id>                    List turns/steps in a session
  loomgate fork <step_id> <target_model>         Fork & diff a historical step on another model
  loomgate mcp list                              List aggregated MCP tools & middleware layers
  loomgate mcp call <tool> [json_args]           Invoke a bridged MCP tool
  loomgate cloud status                          Show Cloud Control Plane connection status
  loomgate cloud login <url> <email> <password>  Sign in to Cloud Control Plane & sync team pool
  loomgate cloud sync                            Refresh team channel pool from Cloud Control Plane
  loomgate cloud logout                          Disconnect from Cloud Control Plane

Environment:
  LOOMGATE_URL    Gateway base URL (default: http://127.0.0.1:8787)
  LOOMGATE_TOKEN  Optional local inbound auth token
`);
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length === 0 || args[0] === "-h" || args[0] === "--help") {
    usage();
    process.exit(0);
  }

  const client = new LoomGateClient();
  const [cmd, sub, a1, a2, a3] = args;

  try {
    if (cmd === "status") {
      const [health, channels, middleware, cloud] = await Promise.all([
        client.health(),
        client.channels().catch(() => []),
        client.middleware().catch(() => ({ layers: [], mcp_tools: [] })),
        client.cloudStatus().catch(() => null),
      ]);
      console.log(JSON.stringify({ health, channels, middleware, cloud }, null, 2));
    } else if (cmd === "sessions") {
      console.log(JSON.stringify(await client.sessions(), null, 2));
    } else if (cmd === "steps" && sub) {
      console.log(JSON.stringify(await client.sessionSteps(sub), null, 2));
    } else if (cmd === "fork" && sub && a1) {
      console.log(JSON.stringify(await client.forkStep(sub, a1), null, 2));
    } else if (cmd === "mcp" && sub === "list") {
      console.log(JSON.stringify(await client.middleware(), null, 2));
    } else if (cmd === "mcp" && sub === "call" && a1) {
      const parsed = a2 ? JSON.parse(a2) : {};
      console.log(JSON.stringify(await client.callMcpTool(a1, parsed), null, 2));
    } else if (cmd === "cloud" && sub === "status") {
      console.log(JSON.stringify(await client.cloudStatus(), null, 2));
    } else if (cmd === "cloud" && sub === "login" && a1 && a2 && a3) {
      console.log(JSON.stringify(await client.cloudLogin(a1, a2, a3), null, 2));
    } else if (cmd === "cloud" && sub === "sync") {
      console.log(JSON.stringify(await client.cloudSync(), null, 2));
    } else if (cmd === "cloud" && sub === "logout") {
      console.log(JSON.stringify(await client.cloudLogout(), null, 2));
    } else {
      console.error(`Unknown or incomplete command: ${args.join(" ")}`);
      usage();
      process.exit(1);
    }
  } catch (err) {
    console.error(err instanceof Error ? err.message : String(err));
    process.exit(1);
  }
}

main();
