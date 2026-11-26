import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type TabId = "channels" | "takeover" | "trajectory" | "savings";

interface ChannelSnapshot {
  id: string;
  protocol: string;
  base_url: string;
  enabled: boolean;
  weight: number;
  models: string[];
  proxy_url: string | null;
  fallback_models: string[];
  quota_endpoint: string | null;
  offline_fallback: boolean;
  health: "healthy" | "cooldown" | "quarantined";
  cooldown_remaining_secs: number;
  quarantine_remaining_secs: number;
  inflight: number;
  ewma_ttft_ms: number | null;
  total_requests: number;
}

interface ChannelConfig {
  id: string;
  protocol: "anthropic" | "openai" | "gemini";
  base_url: string;
  api_key: string;
  weight: number;
  enabled: boolean;
  models: string[];
  model_map: Record<string, string>;
  proxy_url?: string | null;
  fallback_models: string[];
  quota_endpoint?: string | null;
  offline_fallback: boolean;
}

interface DiscoveredLocalProvider {
  kind: "ollama" | "lm_studio" | "vllm" | "custom_openai";
  display_name: string;
  suggested_channel_id: string;
  base_url: string;
  port: number;
  models: string[];
  latency_ms: number;
}

interface ModelSavingsRow {
  model: string;
  requests: number;
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  cache_hit_ratio: number;
  aborted_count: number;
  estimated_actual_cost_usd: number;
  estimated_cache_savings_usd: number;
}

interface SavingsSummary {
  window_days: number;
  total_requests: number;
  completed_requests: number;
  aborted_by_client_count: number;
  sticky_hit_count: number;
  total_input_tokens: number;
  total_cache_read_tokens: number;
  total_output_tokens: number;
  avg_cache_hit_ratio: number;
  estimated_actual_cost_usd: number;
  estimated_cache_savings_usd: number;
  estimated_stop_loss_savings_usd: number;
  by_model: ModelSavingsRow[];
}

interface ChannelTestResult {
  ok: boolean;
  channel_id: string;
  status_code: number | null;
  latency_ms: number;
  endpoint: string;
  message: string;
  quota_info?: unknown;
}

interface AuthSettings {
  api_key: string | null;
  admin_password: string | null;
}

interface ClientProfileStatus {
  id: string;
  display_name: string;
  category: string;
  kind: "atomic_takeover" | "setup_wizard";
  description: string;
  custom: boolean;
  config_path: string | null;
  installed: boolean;
  taken_over: boolean;
  has_backup: boolean;
  active_endpoint: string | null;
  wizard_endpoint: string | null;
  wizard_protocol: string | null;
  env_snippet: string | null;
  config_snippet: string | null;
  instructions: string[];
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

const emptyChannelForm = () => ({
  id: "",
  protocol: "openai" as "anthropic" | "openai" | "gemini",
  base_url: "https://api.openai.com",
  api_key: "",
  weight: 100,
  enabled: true,
  modelsText: "gpt-4o, gpt-4o-mini",
  proxy_url: "",
  fallbackText: "",
  quota_endpoint: "",
  offline_fallback: false,
});

export default function App() {
  const [tab, setTab] = useState<TabId>("channels");
  const [bindAddr, setBindAddr] = useState("127.0.0.1:8787");
  const [routeMode, setRouteMode] = useState<"lock" | "stage_aware">("lock");
  const [channels, setChannels] = useState<ChannelSnapshot[]>([]);
  const [channelConfigs, setChannelConfigs] = useState<ChannelConfig[]>([]);
  const [showChannelForm, setShowChannelForm] = useState(false);
  const [chForm, setChForm] = useState(emptyChannelForm);
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResults, setTestResults] = useState<Record<string, ChannelTestResult>>({});

  const [scanningLocal, setScanningLocal] = useState(false);
  const [localCustomPorts, setLocalCustomPorts] = useState("");
  const [discoveredLocals, setDiscoveredLocals] = useState<DiscoveredLocalProvider[]>([]);
  const [importAsOfflineFallback, setImportAsOfflineFallback] = useState(true);

  const [savingsDays, setSavingsDays] = useState<number>(30);
  const [savings, setSavings] = useState<SavingsSummary | null>(null);

  const [profiles, setProfiles] = useState<ClientProfileStatus[]>([]);
  const [selectedWizardId, setSelectedWizardId] = useState<string>("cursor");

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [steps, setSteps] = useState<TrajectoryStep[]>([]);
  const [forkModel, setForkModel] = useState("gpt-4o");
  const [forkingStep, setForkingStep] = useState<string | null>(null);
  const [diffResult, setDiffResult] = useState<ForkDiffResult | null>(null);
  const [diffViewMode, setDiffViewMode] = useState<"unified" | "split">("unified");

  const [autostart, setAutostart] = useState<boolean>(false);
  const [apiKeyInput, setApiKeyInput] = useState<string>("");
  const [adminPasswordInput, setAdminPasswordInput] = useState<string>("");
  const [activeAuth, setActiveAuth] = useState<AuthSettings>({ api_key: null, admin_password: null });
  const [showSecrets, setShowSecrets] = useState<boolean>(false);

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

    const cfgList = await invoke<ChannelConfig[]>("list_channel_configs");
    setChannelConfigs(cfgList);

    const profList = await invoke<ClientProfileStatus[]>("list_client_profiles");
    setProfiles(profList);

    const cloud = await invoke<CloudSessionStatus>("get_cloud_status");
    setCloudStatus(cloud);

    const sessList = await invoke<SessionSummary[]>("list_sessions");
    setSessions(sessList);
    const sav = await invoke<SavingsSummary>("get_savings_summary", { days: savingsDays });
    setSavings(sav);
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
    invoke<AuthSettings>("get_auth_settings")
      .then((auth) => {
        setActiveAuth(auth);
        setApiKeyInput(auth.api_key ?? "");
        setAdminPasswordInput(auth.admin_password ?? "");
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshAll().catch((e) => setNotice(String(e)));
    const timer = setInterval(() => {
      refreshAll().catch(() => {});
    }, 2000);
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
  }, [selectedSession, savingsDays]);

  async function switchMode(mode: "lock" | "stage_aware") {
    if (!isTauri()) return;
    await invoke("set_route_mode", { mode });
    setRouteMode(mode);
    setNotice(`Route mode hot-switched to ${mode}`);
  }

  function openNewChannelForm() {
    setChForm(emptyChannelForm());
    setShowChannelForm(true);
  }

  function openEditChannelForm(id: string) {
    const existing = channelConfigs.find((c) => c.id === id);
    if (!existing) return;
    setChForm({
      id: existing.id,
      protocol: existing.protocol,
      base_url: existing.base_url,
      api_key: existing.api_key,
      weight: existing.weight,
      enabled: existing.enabled,
      modelsText: existing.models.join(", "),
      proxy_url: existing.proxy_url ?? "",
      fallbackText: (existing.fallback_models ?? []).join(", "),
      quota_endpoint: existing.quota_endpoint ?? "",
      offline_fallback: Boolean(existing.offline_fallback),
    });
    setShowChannelForm(true);
  }

  function buildChannelFromForm(): ChannelConfig {
    const splitCsv = (s: string) =>
      s
        .split(",")
        .map((x) => x.trim())
        .filter(Boolean);
    return {
      id: chForm.id.trim(),
      protocol: chForm.protocol,
      base_url: chForm.base_url.trim(),
      api_key: chForm.api_key.trim(),
      weight: Math.max(1, Number(chForm.weight) || 100),
      enabled: chForm.enabled,
      models: splitCsv(chForm.modelsText),
      model_map: {},
      proxy_url: chForm.proxy_url.trim() || null,
      fallback_models: splitCsv(chForm.fallbackText),
      quota_endpoint: chForm.quota_endpoint.trim() || null,
      offline_fallback: chForm.offline_fallback,
    };
  }

  async function handleScanLocalProviders() {
    if (!isTauri()) return;
    setScanningLocal(true);
    try {
      const customPorts = localCustomPorts
        .split(",")
        .map((s) => Number(s.trim()))
        .filter((n) => Number.isInteger(n) && n > 0 && n <= 65535);
      const found = await invoke<DiscoveredLocalProvider[]>("scan_local_providers", {
        customPorts,
      });
      setDiscoveredLocals(found);
      setNotice(
        found.length > 0
          ? `Discovered ${found.length} local inference service(s).`
          : "No local Ollama (11434) / LM Studio (1234) / vLLM (8000) services responded within 350ms."
      );
    } catch (e) {
      setNotice(`Local scan failed: ${String(e)}`);
    } finally {
      setScanningLocal(false);
    }
  }

  async function handleImportLocalProvider(provider: DiscoveredLocalProvider) {
    if (!isTauri()) return;
    try {
      const ch = await invoke<ChannelSnapshot>("import_local_provider", {
        provider,
        channelId: null,
        offlineFallback: importAsOfflineFallback,
      });
      await refreshAll();
      setNotice(
        `Imported '${ch.id}' (${ch.models.length} model(s))${
          ch.offline_fallback ? " as Offline Fallback" : ""
        } and persisted to config.local.toml.`
      );
    } catch (e) {
      setNotice(`Import local provider failed: ${String(e)}`);
    }
  }

  async function handleSaveChannel() {
    if (!isTauri()) return;
    try {
      const payload = buildChannelFromForm();
      const saved = await invoke<ChannelSnapshot>("upsert_channel", { channel: payload });
      setShowChannelForm(false);
      await refreshAll();
      setNotice(
        `Saved channel '${saved.id}' (${saved.base_url}) and persisted atomically to config.local.toml.`
      );
    } catch (e) {
      setNotice(`Save channel failed: ${String(e)}`);
    }
  }

  async function handleDeleteChannel(id: string) {
    if (!isTauri()) return;
    try {
      await invoke<boolean>("delete_channel", { id });
      await refreshAll();
      setNotice(`Removed channel '${id}' from live pool and config.local.toml.`);
    } catch (e) {
      setNotice(`Delete channel failed: ${String(e)}`);
    }
  }

  async function handleToggleChannel(id: string, enabled: boolean) {
    if (!isTauri()) return;
    try {
      await invoke<ChannelSnapshot>("toggle_channel", { id, enabled });
      await refreshAll();
      setNotice(`Channel '${id}' ${enabled ? "enabled" : "disabled"}.`);
    } catch (e) {
      setNotice(`Toggle channel failed: ${String(e)}`);
    }
  }

  async function handleTestChannel(cfg: ChannelConfig) {
    if (!isTauri()) return;
    setTestingId(cfg.id || "__draft__");
    try {
      const res = await invoke<ChannelTestResult>("test_channel_connection", { channel: cfg });
      setTestResults((prev) => ({ ...prev, [cfg.id || "__draft__"]: res }));
      setNotice(
        `Probe '${cfg.id || "draft"}': ${res.ok ? "OK" : "FAIL"} (${res.latency_ms} ms) — ${res.message}`
      );
    } catch (e) {
      setNotice(`Probe failed: ${String(e)}`);
    } finally {
      setTestingId(null);
    }
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

  async function saveAuthSettings() {
    if (!isTauri()) return;
    try {
      const saved = await invoke<AuthSettings>("set_auth_settings", {
        apiKey: apiKeyInput.trim() || null,
        adminPassword: adminPasswordInput.trim() || null,
      });
      setActiveAuth(saved);
      setApiKeyInput(saved.api_key ?? "");
      setAdminPasswordInput(saved.admin_password ?? "");
      await refreshAll();
      setNotice(
        `Auth settings saved to config.local.toml (API Key: ${
          saved.api_key ? "Protected" : "Open"
        }, Admin Password: ${saved.admin_password ? "Protected" : "Open"}).`
      );
    } catch (e) {
      setNotice(`Save auth settings failed: ${String(e)}`);
    }
  }

  function generateRandomSecret(prefix: string, setter: (v: string) => void) {
    const bytes = new Uint8Array(18);
    crypto.getRandomValues(bytes);
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    setter(`${prefix}_${hex}`);
    setShowSecrets(true);
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
      setNotice(
        `Connected to Cloud Control Plane (${st.tenant_id}, ${st.synced_channels} team channels synced).`
      );
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

  async function handleTakeoverProfile(profileId: string) {
    if (!isTauri()) return;
    try {
      await invoke("takeover_client_profile", { profileId });
      await refreshAll();
      setNotice(`Atomically took over '${profileId}' config (backup created).`);
    } catch (e) {
      setNotice(`Takeover failed: ${String(e)}`);
    }
  }

  async function handleRestoreProfile(profileId: string) {
    if (!isTauri()) return;
    try {
      await invoke("restore_client_profile", { profileId });
      await refreshAll();
      setNotice(`Restored original '${profileId}' config from backup.`);
    } catch (e) {
      setNotice(`Restore failed: ${String(e)}`);
    }
  }

  async function copyText(label: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setNotice(`Copied ${label} to clipboard.`);
    } catch {
      setNotice(`Clipboard unavailable — select and copy ${label} manually.`);
    }
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

  const atomicProfiles = profiles.filter((p) => p.kind === "atomic_takeover");
  const wizardProfiles = profiles.filter((p) => p.kind === "setup_wizard");
  const activeWizard =
    wizardProfiles.find((p) => p.id === selectedWizardId) || wizardProfiles[0] || null;

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
            Client Integrations
          </button>
          <button
            className={`tab ${tab === "trajectory" ? "active" : ""}`}
            onClick={() => setTab("trajectory")}
          >
            Trajectory Fork &amp; Diff
          </button>
          <button
            className={`tab ${tab === "savings" ? "active" : ""}`}
            onClick={() => setTab("savings")}
          >
            Savings &amp; Telemetry
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
                  Hot-switch live without restarting active CLI terminals or IDE agents.
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
              <div className="row" style={{ marginBottom: 12, flexWrap: "wrap" }}>
                <h3 style={{ margin: 0 }}>P2C Upstream Channel Pool (Hot-Editable → config.local.toml)</h3>
                <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
                  <input
                    style={{ width: 160 }}
                    value={localCustomPorts}
                    onChange={(e) => setLocalCustomPorts(e.target.value)}
                    placeholder="Extra ports (e.g. 8080)"
                  />
                  <button
                    className="btn secondary"
                    disabled={scanningLocal}
                    onClick={handleScanLocalProviders}
                  >
                    {scanningLocal ? "Scanning..." : "Scan Local Models (Ollama / LM Studio / vLLM)"}
                  </button>
                  <button className="btn" onClick={openNewChannelForm}>
                    + Add Channel
                  </button>
                </div>
              </div>

              {discoveredLocals.length > 0 && (
                <div
                  className="card"
                  style={{ borderColor: "var(--ok)", background: "var(--bg)", marginBottom: 14 }}
                >
                  <div className="row" style={{ marginBottom: 8, flexWrap: "wrap" }}>
                    <strong>Discovered Local Inference Runtimes</strong>
                    <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 12 }}>
                      <input
                        type="checkbox"
                        checked={importAsOfflineFallback}
                        onChange={(e) => setImportAsOfflineFallback(e.target.checked)}
                      />
                      Import as Offline Fallback (auto-takes over if cloud channels trip/offline)
                    </label>
                  </div>
                  <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
                    {discoveredLocals.map((loc) => (
                      <div key={`${loc.kind}-${loc.port}`} className="row" style={{ flexWrap: "wrap" }}>
                        <div>
                          <strong>{loc.display_name}</strong>{" "}
                          <span className="pill ok">{loc.latency_ms} ms</span>{" "}
                          <span style={{ color: "var(--muted)", fontFamily: "monospace", fontSize: 12 }}>
                            {loc.base_url}
                          </span>
                          <div style={{ fontSize: 12, color: "var(--muted)", marginTop: 2 }}>
                            Models ({loc.models.length}):{" "}
                            {loc.models.length > 0 ? loc.models.join(", ") : "(none reported)"}
                          </div>
                        </div>
                        <button className="btn" onClick={() => handleImportLocalProvider(loc)}>
                          One-Click Import
                        </button>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {showChannelForm && (
                <div
                  className="card"
                  style={{ borderColor: "var(--accent)", background: "var(--bg)", marginBottom: 14 }}
                >
                  <div className="row" style={{ marginBottom: 10 }}>
                    <strong>{chForm.id ? `Edit Channel: ${chForm.id}` : "Add Upstream Channel"}</strong>
                    <button className="btn secondary" onClick={() => setShowChannelForm(false)}>
                      Cancel
                    </button>
                  </div>
                  <div
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                      gap: 10,
                    }}
                  >
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>Channel ID</span>
                      <input
                        value={chForm.id}
                        onChange={(e) => setChForm({ ...chForm, id: e.target.value })}
                        placeholder="openai-live"
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>Protocol</span>
                      <select
                        value={chForm.protocol}
                        onChange={(e) =>
                          setChForm({
                            ...chForm,
                            protocol: e.target.value as "anthropic" | "openai" | "gemini",
                          })
                        }
                      >
                        <option value="anthropic">Anthropic (/v1/messages)</option>
                        <option value="openai">OpenAI (/v1/chat/completions)</option>
                        <option value="gemini">Gemini (/v1beta/models)</option>
                      </select>
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>
                        Base URL (trailing /v1 auto-stripped)
                      </span>
                      <input
                        value={chForm.base_url}
                        onChange={(e) => setChForm({ ...chForm, base_url: e.target.value })}
                        placeholder="https://api.openai.com"
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>Upstream API Key</span>
                      <input
                        type="password"
                        value={chForm.api_key}
                        onChange={(e) => setChForm({ ...chForm, api_key: e.target.value })}
                        placeholder="sk-..."
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>Models (comma-separated)</span>
                      <input
                        value={chForm.modelsText}
                        onChange={(e) => setChForm({ ...chForm, modelsText: e.target.value })}
                        placeholder="gpt-4o, gpt-4o-mini"
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>Weight</span>
                      <input
                        type="number"
                        min={1}
                        value={chForm.weight}
                        onChange={(e) => setChForm({ ...chForm, weight: Number(e.target.value) })}
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>
                        Proxy URL (optional HTTP / SOCKS5)
                      </span>
                      <input
                        value={chForm.proxy_url}
                        onChange={(e) => setChForm({ ...chForm, proxy_url: e.target.value })}
                        placeholder="socks5h://127.0.0.1:1080"
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>
                        Fallback Models (Stage-Aware CSV)
                      </span>
                      <input
                        value={chForm.fallbackText}
                        onChange={(e) => setChForm({ ...chForm, fallbackText: e.target.value })}
                        placeholder="gpt-4o-mini"
                      />
                    </label>
                    <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                      <span style={{ fontSize: 12, color: "var(--muted)" }}>
                        Quota Probe Endpoint (optional)
                      </span>
                      <input
                        value={chForm.quota_endpoint}
                        onChange={(e) => setChForm({ ...chForm, quota_endpoint: e.target.value })}
                        placeholder="https://provider/v1/balance"
                      />
                    </label>
                  </div>
                  <div className="row" style={{ marginTop: 12, flexWrap: "wrap" }}>
                    <div style={{ display: "flex", gap: 14, flexWrap: "wrap" }}>
                      <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 13 }}>
                        <input
                          type="checkbox"
                          checked={chForm.enabled}
                          onChange={(e) => setChForm({ ...chForm, enabled: e.target.checked })}
                        />
                        Enabled in P2C pool
                      </label>
                      <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 13 }}>
                        <input
                          type="checkbox"
                          checked={chForm.offline_fallback}
                          onChange={(e) =>
                            setChForm({ ...chForm, offline_fallback: e.target.checked })
                          }
                        />
                        Offline Fallback (when cloud channels are down)
                      </label>
                    </div>
                    <div style={{ display: "flex", gap: 8 }}>
                      <button
                        className="btn secondary"
                        disabled={testingId !== null}
                        onClick={() => handleTestChannel(buildChannelFromForm())}
                      >
                        Test Connection
                      </button>
                      <button className="btn" onClick={handleSaveChannel}>
                        Save &amp; Persist
                      </button>
                    </div>
                  </div>
                </div>
              )}

              <table>
                <thead>
                  <tr>
                    <th>Channel ID</th>
                    <th>Protocol / Endpoint</th>
                    <th>Health &amp; Cooldown</th>
                    <th>Inflight</th>
                    <th>EWMA TTFT</th>
                    <th>Total Req</th>
                    <th>Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {channels.map((ch) => {
                    const cfg = channelConfigs.find((c) => c.id === ch.id);
                    const probe = testResults[ch.id];
                    const healthLabel = !ch.enabled
                      ? "disabled"
                      : ch.health === "cooldown"
                      ? `cooldown (${ch.cooldown_remaining_secs}s)`
                      : ch.health === "quarantined"
                      ? `quarantined (${ch.quarantine_remaining_secs}s)`
                      : "healthy";
                    return (
                      <tr key={ch.id} style={{ opacity: ch.enabled ? 1 : 0.6 }}>
                        <td>
                          <strong>{ch.id}</strong>{" "}
                          {ch.offline_fallback && <span className="pill ok">Offline Fallback</span>}
                          {ch.proxy_url && (
                            <div style={{ fontSize: 11, color: "var(--muted)" }}>
                              proxy: {ch.proxy_url}
                            </div>
                          )}
                        </td>
                        <td>
                          <span className="pill">{ch.protocol}</span>
                          <div
                            style={{
                              fontSize: 11,
                              color: "var(--muted)",
                              fontFamily: "monospace",
                              marginTop: 2,
                            }}
                          >
                            {ch.base_url}
                          </div>
                        </td>
                        <td>
                          <span
                            className={`pill ${
                              !ch.enabled
                                ? ""
                                : ch.health === "healthy"
                                ? "ok"
                                : ch.health === "cooldown"
                                ? "warn"
                                : "err"
                            }`}
                          >
                            {healthLabel}
                          </span>
                          {probe && (
                            <div style={{ fontSize: 11, color: probe.ok ? "var(--ok)" : "var(--err)", marginTop: 2 }}>
                              {probe.ok ? "✓" : "✗"} {probe.latency_ms}ms · {probe.message}
                            </div>
                          )}
                        </td>
                        <td>{ch.inflight}</td>
                        <td>{ch.ewma_ttft_ms !== null ? `${ch.ewma_ttft_ms} ms` : "—"}</td>
                        <td>{ch.total_requests}</td>
                        <td style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                          {cfg && (
                            <button
                              className="btn secondary"
                              disabled={testingId === ch.id}
                              onClick={() => handleTestChannel(cfg)}
                            >
                              {testingId === ch.id ? "Testing..." : "Test"}
                            </button>
                          )}
                          <button
                            className="btn secondary"
                            onClick={() => handleToggleChannel(ch.id, !ch.enabled)}
                          >
                            {ch.enabled ? "Disable" : "Enable"}
                          </button>
                          <button
                            className="btn secondary"
                            onClick={() => openEditChannelForm(ch.id)}
                          >
                            Edit
                          </button>
                          <button
                            className="btn secondary"
                            onClick={() => handleDeleteChannel(ch.id)}
                          >
                            Delete
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                  {channels.length === 0 && (
                    <tr>
                      <td colSpan={7} style={{ color: "var(--muted)" }}>
                        No channels loaded. Click "+ Add Channel" to configure your first upstream.
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>

            <div className="card">
              <h3>System &amp; Split Local Security</h3>
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
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))",
                  gap: 12,
                  marginBottom: 10,
                }}
              >
                <div>
                  <div style={{ marginBottom: 4 }}>
                    <strong>Inference API Key</strong>{" "}
                    <span className={`pill ${activeAuth.api_key ? "ok" : ""}`}>
                      {activeAuth.api_key ? "Protected (/v1/*, /v1beta/*)" : "Open Localhost"}
                    </span>
                  </div>
                  <div style={{ display: "flex", gap: 6 }}>
                    <input
                      type={showSecrets ? "text" : "password"}
                      style={{ flex: 1, fontFamily: "monospace" }}
                      value={apiKeyInput}
                      onChange={(e) => setApiKeyInput(e.target.value)}
                      placeholder="Empty = passwordless localhost inference"
                    />
                    <button
                      className="btn secondary"
                      onClick={() => generateRandomSecret("lg", setApiKeyInput)}
                    >
                      Gen
                    </button>
                  </div>
                </div>

                <div>
                  <div style={{ marginBottom: 4 }}>
                    <strong>Admin Password</strong>{" "}
                    <span className={`pill ${activeAuth.admin_password ? "ok" : ""}`}>
                      {activeAuth.admin_password ? "Protected (/api/*)" : "Open Localhost"}
                    </span>
                  </div>
                  <div style={{ display: "flex", gap: 6 }}>
                    <input
                      type={showSecrets ? "text" : "password"}
                      style={{ flex: 1, fontFamily: "monospace" }}
                      value={adminPasswordInput}
                      onChange={(e) => setAdminPasswordInput(e.target.value)}
                      placeholder="Empty = passwordless localhost admin"
                    />
                    <button
                      className="btn secondary"
                      onClick={() => generateRandomSecret("lgadm", setAdminPasswordInput)}
                    >
                      Gen
                    </button>
                  </div>
                </div>
              </div>
              <div className="row">
                <span style={{ color: "var(--muted)", fontSize: 12 }}>
                  Inference accepts <code>x-api-key</code> / <code>Bearer</code> /{" "}
                  <code>x-goog-api-key</code>. Admin APIs accept <code>x-admin-password</code> /{" "}
                  <code>Bearer</code>.
                </span>
                <div style={{ display: "flex", gap: 8 }}>
                  <button
                    className="btn secondary"
                    onClick={() => setShowSecrets((v) => !v)}
                  >
                    {showSecrets ? "Hide" : "Show"}
                  </button>
                  <button className="btn" onClick={saveAuthSettings}>
                    Save Auth Settings
                  </button>
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
          <>
            <div className="card">
              <div className="row" style={{ marginBottom: 8 }}>
                <h3 style={{ margin: 0 }}>
                  Atomic CLI Config Takeover (Auto .bak Rollback)
                </h3>
                <span style={{ color: "var(--muted)", fontSize: 12 }}>
                  Extensible via <code>~/.loomgate/client_profiles.json</code> or{" "}
                  <code>.toml</code>
                </span>
              </div>
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
                  {atomicProfiles.map((c) => (
                    <tr key={c.id}>
                      <td>
                        <strong>{c.display_name}</strong>{" "}
                        {c.custom && <span className="pill">Custom</span>}
                      </td>
                      <td style={{ color: "var(--muted)", fontFamily: "monospace", fontSize: 12 }}>
                        {c.config_path}
                      </td>
                      <td>
                        <span className={`pill ${c.taken_over ? "ok" : ""}`}>
                          {c.taken_over ? "Proxied" : "Direct"}
                        </span>
                      </td>
                      <td style={{ display: "flex", gap: 8 }}>
                        <button className="btn" onClick={() => handleTakeoverProfile(c.id)}>
                          {c.taken_over ? "Re-Apply" : "Take Over"}
                        </button>
                        {c.has_backup && (
                          <button
                            className="btn secondary"
                            onClick={() => handleRestoreProfile(c.id)}
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

            <div className="card">
              <h3>IDE &amp; SDK Setup Wizards (One-Click Copy)</h3>
              <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 14 }}>
                {wizardProfiles.map((w) => (
                  <button
                    key={w.id}
                    className={`btn ${activeWizard?.id === w.id ? "" : "secondary"}`}
                    onClick={() => setSelectedWizardId(w.id)}
                  >
                    {w.display_name}
                    {w.custom ? " (Custom)" : ""}
                  </button>
                ))}
              </div>
              {activeWizard && (
                <div>
                  <div className="row" style={{ marginBottom: 8 }}>
                    <div>
                      <strong>{activeWizard.display_name}</strong>{" "}
                      <span className="pill">{activeWizard.category}</span>{" "}
                      {activeWizard.wizard_protocol && (
                        <span className="pill ok">{activeWizard.wizard_protocol}</span>
                      )}
                    </div>
                    {activeWizard.wizard_endpoint && (
                      <button
                        className="btn secondary"
                        onClick={() => copyText("Endpoint URL", activeWizard.wizard_endpoint!)}
                      >
                        Copy URL: {activeWizard.wizard_endpoint}
                      </button>
                    )}
                  </div>
                  <p style={{ color: "var(--muted)", margin: "0 0 10px" }}>
                    {activeWizard.description}
                  </p>
                  <ol style={{ margin: "0 0 12px", paddingLeft: 20, color: "var(--text)" }}>
                    {activeWizard.instructions.map((step, idx) => (
                      <li key={idx} style={{ marginBottom: 4 }}>
                        {step}
                      </li>
                    ))}
                  </ol>
                  <div className="diff-grid">
                    {activeWizard.env_snippet && (
                      <div>
                        <div className="row" style={{ marginBottom: 4 }}>
                          <span style={{ fontSize: 12, color: "var(--muted)" }}>
                            Environment Variables
                          </span>
                          <button
                            className="btn secondary"
                            onClick={() =>
                              copyText(`${activeWizard.display_name} env`, activeWizard.env_snippet!)
                            }
                          >
                            Copy Env
                          </button>
                        </div>
                        <pre>{activeWizard.env_snippet}</pre>
                      </div>
                    )}
                    {activeWizard.config_snippet && (
                      <div>
                        <div className="row" style={{ marginBottom: 4 }}>
                          <span style={{ fontSize: 12, color: "var(--muted)" }}>
                            JSON Config Snippet
                          </span>
                          <button
                            className="btn secondary"
                            onClick={() =>
                              copyText(
                                `${activeWizard.display_name} JSON`,
                                activeWizard.config_snippet!
                              )
                            }
                          >
                            Copy JSON
                          </button>
                        </div>
                        <pre>{activeWizard.config_snippet}</pre>
                      </div>
                    )}
                  </div>
                </div>
              )}
            </div>
          </>
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

        {tab === "savings" && (
          <>
            <div className="card row">
              <div>
                <h3 style={{ margin: 0 }}>Local Prompt Caching Savings &amp; Stop-Loss Telemetry</h3>
                <span style={{ color: "var(--muted)", fontSize: 12 }}>
                  Computed 100% locally from SQLite WAL trajectories — zero raw prompts leave your machine.
                </span>
              </div>
              <div style={{ display: "flex", gap: 8 }}>
                {[
                  { label: "7 Days", days: 7 },
                  { label: "30 Days", days: 30 },
                  { label: "All Time", days: 0 },
                ].map((w) => (
                  <button
                    key={w.days}
                    className={`btn ${savingsDays === w.days ? "" : "secondary"}`}
                    onClick={() => setSavingsDays(w.days)}
                  >
                    {w.label}
                  </button>
                ))}
              </div>
            </div>

            <div className="kpi-grid">
              <div className="kpi-card">
                <div className="kpi-label">Total Requests</div>
                <div className="kpi-value">{savings?.total_requests ?? 0}</div>
                <div className="kpi-sub">
                  Sticky hits: {savings?.sticky_hit_count ?? 0} · Completed:{" "}
                  {savings?.completed_requests ?? 0}
                </div>
              </div>
              <div className="kpi-card">
                <div className="kpi-label">Prompt Cache Hit Rate</div>
                <div className="kpi-value" style={{ color: "var(--ok)" }}>
                  {((savings?.avg_cache_hit_ratio ?? 0) * 100).toFixed(1)}%
                </div>
                <div className="kpi-sub">
                  Cache read tokens: {(savings?.total_cache_read_tokens ?? 0).toLocaleString()}
                </div>
              </div>
              <div className="kpi-card">
                <div className="kpi-label">Estimated Cache Savings</div>
                <div className="kpi-value" style={{ color: "var(--ok)" }}>
                  ${(savings?.estimated_cache_savings_usd ?? 0).toFixed(4)}
                </div>
                <div className="kpi-sub">
                  Est. actual spend: ${(savings?.estimated_actual_cost_usd ?? 0).toFixed(4)}
                </div>
              </div>
              <div className="kpi-card">
                <div className="kpi-label">Ctrl+C Stop-Loss Aborts</div>
                <div className="kpi-value" style={{ color: "var(--warn)" }}>
                  {savings?.aborted_by_client_count ?? 0}
                </div>
                <div className="kpi-sub">
                  Est. stop-loss saved: ${(savings?.estimated_stop_loss_savings_usd ?? 0).toFixed(4)}
                </div>
              </div>
            </div>

            <div className="card">
              <h3>Model Distribution &amp; Cache Hit Efficiency</h3>
              {savings && savings.by_model.length > 0 ? (
                <>
                  <svg
                    width="100%"
                    height={savings.by_model.length * 36 + 16}
                    style={{ display: "block", marginBottom: 14 }}
                  >
                    {(() => {
                      const maxReq = Math.max(...savings.by_model.map((m) => m.requests), 1);
                      return savings.by_model.map((m, idx) => {
                        const y = idx * 36 + 8;
                        const pctWidth = Math.max(4, Math.round((m.requests / maxReq) * 55));
                        const hitPct = Math.round(m.cache_hit_ratio * 100);
                        return (
                          <g key={m.model} transform={`translate(0, ${y})`}>
                            <text x="0" y="16" fill="#e6edf3" fontSize="12" fontFamily="monospace">
                              {m.model.slice(0, 28)}
                            </text>
                            <rect
                              x="230"
                              y="5"
                              width={`${pctWidth}%`}
                              height="14"
                              rx="4"
                              fill={hitPct >= 50 ? "#3fb950" : "#388bfd"}
                            />
                            <text x="82%" y="16" fill="#8b949e" fontSize="12">
                              {m.requests} req · KV {hitPct}% · ${m.estimated_cache_savings_usd.toFixed(4)} saved
                            </text>
                          </g>
                        );
                      });
                    })()}
                  </svg>
                  <table>
                    <thead>
                      <tr>
                        <th>Model</th>
                        <th>Requests</th>
                        <th>Input Tokens</th>
                        <th>Cache Read Tokens</th>
                        <th>KV Hit %</th>
                        <th>Aborts</th>
                        <th>Est. Cache Saved</th>
                      </tr>
                    </thead>
                    <tbody>
                      {savings.by_model.map((row) => (
                        <tr key={row.model}>
                          <td style={{ fontFamily: "monospace", fontSize: 12 }}>{row.model}</td>
                          <td>{row.requests}</td>
                          <td>{row.input_tokens.toLocaleString()}</td>
                          <td>{row.cache_read_tokens.toLocaleString()}</td>
                          <td style={{ minWidth: 110 }}>
                            <div style={{ fontSize: 12, marginBottom: 3 }}>
                              {(row.cache_hit_ratio * 100).toFixed(1)}%
                            </div>
                            <div className="bar-track">
                              <div
                                className={`bar-fill ${row.cache_hit_ratio >= 0.5 ? "ok" : ""}`}
                                style={{ width: `${Math.round(row.cache_hit_ratio * 100)}%` }}
                              />
                            </div>
                          </td>
                          <td>{row.aborted_count}</td>
                          <td style={{ color: "var(--ok)" }}>
                            ${row.estimated_cache_savings_usd.toFixed(4)}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </>
              ) : (
                <p style={{ color: "var(--muted)" }}>
                  No trajectory samples in this window yet. Route requests through {bindAddr} to
                  see real-time cache savings and stop-loss telemetry.
                </p>
              )}
            </div>
          </>
        )}
      </main>
    </div>
  );
}
