import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type TabId = "channels" | "takeover" | "trajectory";

interface ChannelSnapshot {
  id: string;
  protocol: string;
  health: "healthy" | "cooldown" | "quarantined";
  inflight: number;
  ewma_ttft_ms: number | null;
  total_requests: number;
}

interface CliIntegrationStatus {
  tool: "claude_code" | "codex" | "gemini_cli";
  display_name: string;
  config_path: string;
  installed: boolean;
  taken_over: boolean;
  has_backup: boolean;
  active_endpoint: string | null;
}

interface SessionSummary {
  session_id: string;
  workspace: string;
  step_count: number;
  fork_count: number;
  total_cache_read_tokens: number;
  total_input_tokens: number;
  last_model: string;
  updated_at_ms: number;
}

interface TrajectoryStep {
  step_id: string;
  session_id: string;
  parent_step_id: string | null;
  turn_index: number;
  channel_id: string;
  actual_model: string;
  route_mode: string;
  sticky_hit: boolean;
  ttft_ms: number;
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  status: string;
  response_text: string;
}

interface ForkDiffResult {
  original: TrajectoryStep;
  forked: TrajectoryStep;
  ttft_delta_ms: number;
  output_tokens_delta: number;
  cache_hit_ratio_delta: number;
}

interface CloudSessionStatus {
  connected: boolean;
  control_plane_url: string | null;
  email: string | null;
  tenant_id: string | null;
  synced_channels: number;
  available_micros: number | null;
  telemetry_uploaded: number;
  last_sync_ms: number | null;
}

interface DiffLine {
  kind: "same" | "del" | "add";
  text: string;
  oldLine?: number;
  newLine?: number;
}

function computeLineDiff(oldText: string, newText: string): DiffLine[] {
  const a = oldText.split(/\r?\n/);
  const b = newText.split(/\r?\n/);
  const m = a.length;
  const n = b.length;
  const dp: number[][] = Array.from({ length: m + 1 }, () => Array(n + 1).fill(0));
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const result: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < m && j < n) {
    if (a[i] === b[j]) {
      result.push({ kind: "same", text: a[i], oldLine: i + 1, newLine: j + 1 });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      result.push({ kind: "del", text: a[i], oldLine: i + 1 });
      i++;
    } else {
      result.push({ kind: "add", text: b[j], newLine: j + 1 });
      j++;
    }
  }
  while (i < m) {
    result.push({ kind: "del", text: a[i], oldLine: i + 1 });
    i++;
  }
  while (j < n) {
    result.push({ kind: "add", text: b[j], newLine: j + 1 });
    j++;
  }
  return result;
}

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export default function App() {
  const [tab, setTab] = useState<TabId>("channels");
  const [bindAddr, setBindAddr] = useState("127.0.0.1:8787");
  const [routeMode, setRouteMode] = useState<"lock" | "stage_aware">("lock");
  const [channels, setChannels] = useState<ChannelSnapshot[]>([]);
  const [clis, setClis] = useState<CliIntegrationStatus[]>([]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [steps, setSteps] = useState<TrajectoryStep[]>([]);
  const [forkModel, setForkModel] = useState("gpt-4o");
  const [forkingStep, setForkingStep] = useState<string | null>(null);
  const [diffResult, setDiffResult] = useState<ForkDiffResult | null>(null);
  const [diffViewMode, setDiffViewMode] = useState<"unified" | "split">("unified");
  const [autostart, setAutostart] = useState<boolean>(false);
  const [authTokenInput, setAuthTokenInput] = useState<string>("");
  const [activeAuthToken, setActiveAuthToken] = useState<string | null>(null);
  const [showAuthToken, setShowAuthToken] = useState<boolean>(false);
  const [cloudStatus, setCloudStatus] = useState<CloudSessionStatus | null>(null);
  const [cloudUrl, setCloudUrl] = useState("http://127.0.0.1:8790");
  const [cloudEmail, setCloudEmail] = useState("dev@loomgate.dev");
  const [cloudPassword, setCloudPassword] = useState("loomgate-dev-password");
  const [notice, setNotice] = useState<string>("");

  async function refreshAll() {
    if (!isTauri()) return;
    const status = await invoke<{
      bind_addr: string;
      route_mode: "lock" | "stage_aware";
      channels: ChannelSnapshot[];
    }>("get_gateway_status");
    setBindAddr(status.bind_addr);
    setRouteMode(status.route_mode);
    setChannels(status.channels);

    const cliList = await invoke<CliIntegrationStatus[]>("list_cli_integrations");
    setClis(cliList);

    const cloud = await invoke<CloudSessionStatus>("get_cloud_status");
    setCloudStatus(cloud);

    const sessList = await invoke<SessionSummary[]>("list_sessions");
    setSessions(sessList);
    if (selectedSession) {
      const st = await invoke<TrajectoryStep[]>("get_session_steps", {
        sessionId: selectedSession,
      });
      setSteps(st);
    }
  }

  useEffect(() => {
    if (!isTauri()) return;
    invoke<boolean>("get_autostart_enabled")
      .then(setAutostart)
      .catch(() => {});
    invoke<string | null>("get_local_auth_token")
      .then((tok) => {
        setActiveAuthToken(tok);
        setAuthTokenInput(tok ?? "");
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshAll().catch((e) => setNotice(String(e)));
    const timer = setInterval(() => {
      refreshAll().catch(() => {});
    }, 3000);
    let unlisten: (() => void) | undefined;
    if (isTauri()) {
      listen<"lock" | "stage_aware">("route-mode-changed", (event) => {
        setRouteMode(event.payload);
        setNotice(`Tray hot-switched route mode to ${event.payload}`);
      })
        .then((fn) => {
          unlisten = fn;
        })
        .catch(() => {});
    }
    return () => {
      clearInterval(timer);
      if (unlisten) unlisten();
    };
  }, [selectedSession]);

  async function switchMode(mode: "lock" | "stage_aware") {
    if (!isTauri()) return;
    await invoke("set_route_mode", { mode });
    setRouteMode(mode);
    setNotice(`Route mode hot-switched to ${mode}`);
  }

  async function toggleAutostart() {
    if (!isTauri()) return;
    try {
      const next = await invoke<boolean>("set_autostart_enabled", { enabled: !autostart });
      setAutostart(next);
      setNotice(next ? "Launch at login enabled (Windows Run key)." : "Launch at login disabled.");
    } catch (e) {
      setNotice(`Autostart update failed: ${String(e)}`);
    }
  }

  async function saveAuthToken() {
    if (!isTauri()) return;
    const saved = await invoke<string | null>("set_local_auth_token", {
      token: authTokenInput.trim() || null,
    });
    setActiveAuthToken(saved);
    setAuthTokenInput(saved ?? "");
    setNotice(
      saved
        ? "Local inbound auth token active (enforced for x-api-key / Bearer / x-goog-api-key)."
        : "Local inbound auth disabled (open localhost dev mode)."
    );
  }

  async function clearAuthToken() {
    if (!isTauri()) return;
    await invoke<string | null>("set_local_auth_token", { token: null });
    setActiveAuthToken(null);
    setAuthTokenInput("");
    setNotice("Local inbound auth token cleared.");
  }

  function generateRandomToken() {
    const bytes = new Uint8Array(18);
    crypto.getRandomValues(bytes);
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    setAuthTokenInput(`lg_${hex}`);
    setShowAuthToken(true);
  }

  async function handleCloudLogin() {
    if (!isTauri()) return;
    try {
      const st = await invoke<CloudSessionStatus>("cloud_login", {
        controlPlaneUrl: cloudUrl,
        email: cloudEmail,
        password: cloudPassword,
      });
      setCloudStatus(st);
      await refreshAll();
      setNotice(`Connected to Cloud Control Plane (${st.tenant_id}, ${st.synced_channels} team channels synced).`);
    } catch (e) {
      setNotice(`Cloud login failed: ${String(e)}`);
    }
  }

  async function handleCloudSync() {
    if (!isTauri()) return;
    try {
      const st = await invoke<CloudSessionStatus>("cloud_sync");
      setCloudStatus(st);
      await refreshAll();
      setNotice(`Synced ${st.synced_channels} team channels from Cloud Control Plane.`);
    } catch (e) {
      setNotice(`Cloud sync failed: ${String(e)}`);
    }
  }

  async function handleCloudLogout() {
    if (!isTauri()) return;
    const st = await invoke<CloudSessionStatus>("cloud_logout");
    setCloudStatus(st);
    setNotice("Disconnected from Cloud Control Plane (running in local-only privacy mode).");
  }

  async function handleTakeover(tool: CliIntegrationStatus["tool"]) {
    if (!isTauri()) return;
    await invoke("takeover_cli_config", { tool });
    await refreshAll();
    setNotice(`Atomically took over ${tool} config (backup created).`);
  }

  async function handleRestore(tool: CliIntegrationStatus["tool"]) {
    if (!isTauri()) return;
    await invoke("restore_cli_config", { tool });
    await refreshAll();
    setNotice(`Restored original ${tool} config.`);
  }

  async function openSession(sessionId: string) {
    setSelectedSession(sessionId);
    setDiffResult(null);
    if (!isTauri()) return;
    const st = await invoke<TrajectoryStep[]>("get_session_steps", { sessionId });
    setSteps(st);
  }

  async function handleFork(stepId: string) {
    if (!isTauri()) return;
    setForkingStep(stepId);
    try {
      const res = await invoke<ForkDiffResult>("fork_and_diff_step", {
        stepId,
        targetModel: forkModel,
      });
      setDiffResult(res);
      if (selectedSession) {
        const st = await invoke<TrajectoryStep[]>("get_session_steps", {
          sessionId: selectedSession,
        });
        setSteps(st);
      }
    } catch (e) {
      setNotice(`Fork failed: ${String(e)}`);
    } finally {
      setForkingStep(null);
    }
  }

  return (
    <div className="layout">
      <header className="topbar">
        <div className="brand">
          <span className={`status-dot ${routeMode === "lock" ? "lock" : "stage"}`} />
          <span>LoomGate</span>
          <span className="pill ok">{bindAddr}</span>
          <span className="pill">{routeMode === "lock" ? "Lock Mode" : "Stage-Aware"}</span>
        </div>
        <div className="tabs">
          <button
            className={`tab ${tab === "channels" ? "active" : ""}`}
            onClick={() => setTab("channels")}
          >
            Channels &amp; Routing
          </button>
          <button
            className={`tab ${tab === "takeover" ? "active" : ""}`}
            onClick={() => setTab("takeover")}
          >
            CLI Takeover
          </button>
          <button
            className={`tab ${tab === "trajectory" ? "active" : ""}`}
            onClick={() => setTab("trajectory")}
          >
            Trajectory Fork &amp; Diff
          </button>
        </div>
      </header>

      <main className="main">
        {notice && (
          <div className="card row">
            <span>{notice}</span>
            <button className="btn secondary" onClick={() => setNotice("")}>
              Dismiss
            </button>
          </div>
        )}

        {tab === "channels" && (
          <>
            <div className="card row">
              <div>
                <h3>Explicit Routing Policy (Zero Silent Downgrade)</h3>
                <span style={{ color: "var(--muted)" }}>
                  Hot-switch live without restarting active CLI terminals.
                </span>
              </div>
              <div style={{ display: "flex", gap: 8 }}>
                <button
                  className={`btn ${routeMode === "lock" ? "" : "secondary"}`}
                  onClick={() => switchMode("lock")}
                >
                  Lock Mode
                </button>
                <button
                  className={`btn ${routeMode === "stage_aware" ? "" : "secondary"}`}
                  onClick={() => switchMode("stage_aware")}
                >
                  Stage-Aware
                </button>
              </div>
            </div>

            <div className="card">
              <h3>P2C Upstream Channel Pool</h3>
              <table>
                <thead>
                  <tr>
                    <th>Channel ID</th>
                    <th>Protocol</th>
                    <th>Health</th>
                    <th>Inflight</th>
                    <th>EWMA TTFT</th>
                    <th>Total Req</th>
                  </tr>
                </thead>
                <tbody>
                  {channels.map((ch) => (
                    <tr key={ch.id}>
                      <td>{ch.id}</td>
                      <td>{ch.protocol}</td>
                      <td>
                        <span
                          className={`pill ${
                            ch.health === "healthy"
                              ? "ok"
                              : ch.health === "cooldown"
                              ? "warn"
                              : "err"
                          }`}
                        >
                          {ch.health}
                        </span>
                      </td>
                      <td>{ch.inflight}</td>
                      <td>{ch.ewma_ttft_ms !== null ? `${ch.ewma_ttft_ms} ms` : "—"}</td>
                      <td>{ch.total_requests}</td>
                    </tr>
                  ))}
                  {channels.length === 0 && (
                    <tr>
                      <td colSpan={6} style={{ color: "var(--muted)" }}>
                        No channels loaded (launch via `npm run tauri dev` to connect to embedded kernel).
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>

            <div className="card">
              <h3>System &amp; Local Security</h3>
              <div className="row" style={{ marginBottom: 14 }}>
                <div>
                  <strong>Launch LoomGate at Login</strong>
                  <div style={{ color: "var(--muted)", fontSize: 12 }}>
                    Registers current executable under HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
                  </div>
                </div>
                <button
                  className={`btn ${autostart ? "" : "secondary"}`}
                  onClick={toggleAutostart}
                >
                  {autostart ? "Enabled" : "Disabled"}
                </button>
              </div>
              <div className="row" style={{ alignItems: "flex-end", flexWrap: "wrap" }}>
                <div style={{ flex: 1, minWidth: 260 }}>
                  <div style={{ marginBottom: 4 }}>
                    <strong>Local Inbound Auth Token</strong>{" "}
                    <span className={`pill ${activeAuthToken ? "ok" : ""}`}>
                      {activeAuthToken ? "Protected" : "Open Localhost"}
                    </span>
                  </div>
                  <div style={{ color: "var(--muted)", fontSize: 12, marginBottom: 6 }}>
                    When set, inbound requests to {bindAddr} must pass this token via{" "}
                    <code>x-api-key</code>, <code>Authorization: Bearer</code>, or{" "}
                    <code>x-goog-api-key</code>.
                  </div>
                  <input
                    type={showAuthToken ? "text" : "password"}
                    style={{ width: "100%", fontFamily: "monospace" }}
                    value={authTokenInput}
                    onChange={(e) => setAuthTokenInput(e.target.value)}
                    placeholder="Leave empty to allow unauthenticated local CLI requests"
                  />
                </div>
                <div style={{ display: "flex", gap: 8 }}>
                  <button
                    className="btn secondary"
                    onClick={() => setShowAuthToken((v) => !v)}
                  >
                    {showAuthToken ? "Hide" : "Show"}
                  </button>
                  <button className="btn secondary" onClick={generateRandomToken}>
                    Generate
                  </button>
                  <button className="btn" onClick={saveAuthToken}>
                    Save
                  </button>
                  {activeAuthToken && (
                    <button className="btn secondary" onClick={clearAuthToken}>
                      Clear
                    </button>
                  )}
                </div>
              </div>
            </div>

            <div className="card">
              <div className="row" style={{ marginBottom: 10 }}>
                <h3 style={{ margin: 0 }}>Cloud Control Plane (Team Pool &amp; Sanitized Analytics)</h3>
                <span className={`pill ${cloudStatus?.connected ? "ok" : ""}`}>
                  {cloudStatus?.connected
                    ? `Connected · ${cloudStatus.tenant_id}`
                    : "Local-Only Mode"}
                </span>
              </div>
              {cloudStatus?.connected ? (
                <div className="row" style={{ flexWrap: "wrap" }}>
                  <div style={{ color: "var(--muted)", fontSize: 13 }}>
                    Account: <strong>{cloudStatus.email}</strong> · Balance:{" "}
                    <strong>
                      $
                      {cloudStatus.available_micros !== null
                        ? (cloudStatus.available_micros / 1_000_000).toFixed(4)
                        : "0.0000"}
                    </strong>{" "}
                    · Synced Team Channels: <strong>{cloudStatus.synced_channels}</strong> ·
                    Desensitized Events Uploaded: <strong>{cloudStatus.telemetry_uploaded}</strong>
                  </div>
                  <div style={{ display: "flex", gap: 8 }}>
                    <button className="btn secondary" onClick={handleCloudSync}>
                      Sync Team Pool
                    </button>
                    <button className="btn secondary" onClick={handleCloudLogout}>
                      Disconnect
                    </button>
                  </div>
                </div>
              ) : (
                <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
                  <input
                    style={{ minWidth: 200 }}
                    value={cloudUrl}
                    onChange={(e) => setCloudUrl(e.target.value)}
                    placeholder="http://127.0.0.1:8790"
                  />
                  <input
                    style={{ minWidth: 180 }}
                    value={cloudEmail}
                    onChange={(e) => setCloudEmail(e.target.value)}
                    placeholder="dev@loomgate.dev"
                  />
                  <input
                    type="password"
                    style={{ minWidth: 160 }}
                    value={cloudPassword}
                    onChange={(e) => setCloudPassword(e.target.value)}
                    placeholder="password"
                  />
                  <button className="btn" onClick={handleCloudLogin}>
                    Sign In &amp; Sync Pool
                  </button>
                </div>
              )}
            </div>
          </>
        )}

        {tab === "takeover" && (
          <div className="card">
            <h3>One-Click Atomic CLI Config Takeover (CC Switch Pattern)</h3>
            <table>
              <thead>
                <tr>
                  <th>Client</th>
                  <th>Config Path</th>
                  <th>Status</th>
                  <th>Action</th>
                </tr>
              </thead>
              <tbody>
                {clis.map((c) => (
                  <tr key={c.tool}>
                    <td>{c.display_name}</td>
                    <td style={{ color: "var(--muted)", fontFamily: "monospace", fontSize: 12 }}>
                      {c.config_path}
                    </td>
                    <td>
                      <span className={`pill ${c.taken_over ? "ok" : ""}`}>
                        {c.taken_over ? "Proxied" : "Direct"}
                      </span>
                    </td>
                    <td style={{ display: "flex", gap: 8 }}>
                      <button className="btn" onClick={() => handleTakeover(c.tool)}>
                        {c.taken_over ? "Re-Apply" : "Take Over"}
                      </button>
                      {c.has_backup && (
                        <button
                          className="btn secondary"
                          onClick={() => handleRestore(c.tool)}
                        >
                          Restore
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {tab === "trajectory" && (
          <div className="grid-2">
            <div className="card" style={{ overflow: "auto" }}>
              <h3>Local SQLite Sessions</h3>
              {sessions.map((s) => {
                const totalIn = s.total_input_tokens + s.total_cache_read_tokens;
                const hitPct =
                  totalIn > 0 ? Math.round((s.total_cache_read_tokens / totalIn) * 100) : 0;
                return (
                  <div
                    key={s.session_id}
                    className={`session-item ${
                      selectedSession === s.session_id ? "selected" : ""
                    }`}
                    onClick={() => openSession(s.session_id)}
                  >
                    <div className="row">
                      <strong>{s.session_id}</strong>
                      <span className="pill ok">KV {hitPct}%</span>
                    </div>
                    <div style={{ color: "var(--muted)", fontSize: 12, marginTop: 4 }}>
                      {s.last_model} · {s.step_count} steps · {s.fork_count} forks
                    </div>
                  </div>
                );
              })}
              {sessions.length === 0 && (
                <p style={{ color: "var(--muted)" }}>
                  No local trajectories yet. Send requests through {bindAddr} to populate.
                </p>
              )}
            </div>

            <div className="card" style={{ overflow: "auto" }}>
              <div className="row" style={{ marginBottom: 12 }}>
                <h3 style={{ margin: 0 }}>Turn → Step Timeline &amp; Fork/Diff</h3>
                <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <span style={{ color: "var(--muted)", fontSize: 12 }}>Fork target model:</span>
                  <input
                    value={forkModel}
                    onChange={(e) => setForkModel(e.target.value)}
                    placeholder="gpt-4o / gemini-2.5-pro / claude-sonnet-4-20250514"
                  />
                </div>
              </div>

              {diffResult && (
                <div className="card" style={{ borderColor: "var(--accent)" }}>
                  <div className="row" style={{ flexWrap: "wrap" }}>
                    <strong>
                      Fork Diff: {diffResult.original.actual_model} vs{" "}
                      {diffResult.forked.actual_model}
                    </strong>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <span className="pill">
                        TTFT Δ: {diffResult.ttft_delta_ms >= 0 ? "+" : ""}
                        {diffResult.ttft_delta_ms} ms | Output Δ:{" "}
                        {diffResult.output_tokens_delta >= 0 ? "+" : ""}
                        {diffResult.output_tokens_delta} tok
                      </span>
                      <button
                        className={`btn ${diffViewMode === "unified" ? "" : "secondary"}`}
                        onClick={() => setDiffViewMode("unified")}
                      >
                        Line Diff
                      </button>
                      <button
                        className={`btn ${diffViewMode === "split" ? "" : "secondary"}`}
                        onClick={() => setDiffViewMode("split")}
                      >
                        Side-by-Side
                      </button>
                    </div>
                  </div>
                  {diffViewMode === "unified" ? (
                    <div className="diff-box">
                      {computeLineDiff(
                        diffResult.original.response_text || "",
                        diffResult.forked.response_text || ""
                      ).map((dl, idx) => (
                        <div key={idx} className={`diff-line ${dl.kind}`}>
                          <span className="diff-ln">{dl.oldLine ?? ""}</span>
                          <span className="diff-ln">{dl.newLine ?? ""}</span>
                          <span className="diff-sigil">
                            {dl.kind === "add" ? "+" : dl.kind === "del" ? "-" : " "}
                          </span>
                          <span>{dl.text || " "}</span>
                        </div>
                      ))}
                    </div>
                  ) : (
                    <div className="diff-grid">
                      <div>
                        <div style={{ fontSize: 12, color: "var(--muted)" }}>
                          Original ({diffResult.original.actual_model})
                        </div>
                        <pre>{diffResult.original.response_text}</pre>
                      </div>
                      <div>
                        <div style={{ fontSize: 12, color: "var(--muted)" }}>
                          Forked ({diffResult.forked.actual_model})
                        </div>
                        <pre>{diffResult.forked.response_text}</pre>
                      </div>
                    </div>
                  )}
                </div>
              )}

              {steps.map((st) => {
                const totalIn = st.input_tokens + st.cache_read_tokens;
                const hitPct = totalIn > 0 ? Math.round((st.cache_read_tokens / totalIn) * 100) : 0;
                return (
                  <div
                    key={st.step_id}
                    className={`step-card ${st.parent_step_id ? "fork" : ""}`}
                  >
                    <div className="row">
                      <div>
                        <strong>
                          Turn {st.turn_index} {st.parent_step_id ? "(Fork)" : ""}
                        </strong>{" "}
                        <span className="pill">{st.actual_model}</span>{" "}
                        <span className="pill">{st.channel_id}</span>{" "}
                        <span className="pill ok">KV {hitPct}%</span>{" "}
                        <span className="pill">{st.ttft_ms} ms</span>
                      </div>
                      <button
                        className="btn secondary"
                        disabled={forkingStep === st.step_id}
                        onClick={() => handleFork(st.step_id)}
                      >
                        {forkingStep === st.step_id ? "Forking..." : `Fork → ${forkModel}`}
                      </button>
                    </div>
                    <pre style={{ marginTop: 8 }}>{st.response_text || "(empty response)"}</pre>
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </main>
    </div>
  );
}
