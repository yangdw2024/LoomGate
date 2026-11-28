import { useEffect, useState } from "react";
import {
  AuthSettings,
  ChannelConfig,
  ChannelSnapshot,
  ChannelTestResult,
  CloudSessionStatus,
  DiscoveredLocalProvider,
  gatewayClient,
} from "../api/client";

const isDesktop = () => gatewayClient.isTauri();

const emptyChannelForm = () => ({
  id: "",
  protocol: "openai" as ChannelConfig["protocol"],
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

export interface ChannelsPageProps {
  routeMode: "lock" | "stage_aware";
  channels: ChannelSnapshot[];
  channelConfigs: ChannelConfig[];
  autostart: boolean;
  activeAuth: AuthSettings;
  cloudStatus: CloudSessionStatus | null;
  onRouteModeChange: (mode: "lock" | "stage_aware") => void;
  onAutostartChange: (enabled: boolean) => void;
  onActiveAuthChange: (auth: AuthSettings) => void;
  onCloudStatusChange: (status: CloudSessionStatus) => void;
  onRefresh: () => Promise<void>;
  onNotice: (msg: string) => void;
  onError: (err: unknown, prefix?: string) => void;
}

export default function ChannelsPage({
  routeMode,
  channels,
  channelConfigs,
  autostart,
  activeAuth,
  cloudStatus,
  onRouteModeChange,
  onAutostartChange,
  onActiveAuthChange,
  onCloudStatusChange,
  onRefresh,
  onNotice,
  onError,
}: ChannelsPageProps) {
  const [showChannelForm, setShowChannelForm] = useState(false);
  const [chForm, setChForm] = useState(emptyChannelForm);
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResults, setTestResults] = useState<Record<string, ChannelTestResult>>({});

  const [scanningLocal, setScanningLocal] = useState(false);
  const [localCustomPorts, setLocalCustomPorts] = useState("");
  const [discoveredLocals, setDiscoveredLocals] = useState<DiscoveredLocalProvider[]>([]);
  const [importAsOfflineFallback, setImportAsOfflineFallback] = useState(true);

  const [apiKeyInput, setApiKeyInput] = useState<string>(activeAuth.api_key ?? "");
  const [adminPasswordInput, setAdminPasswordInput] = useState<string>(
    activeAuth.admin_password ?? ""
  );
  const [showSecrets, setShowSecrets] = useState<boolean>(false);

  const [cloudUrl, setCloudUrl] = useState("http://127.0.0.1:8790");
  const [cloudEmail, setCloudEmail] = useState("dev@loomgate.dev");
  const [cloudPassword, setCloudPassword] = useState("loomgate-dev-password");

  useEffect(() => {
    setApiKeyInput(activeAuth.api_key ?? "");
    setAdminPasswordInput(activeAuth.admin_password ?? "");
  }, [activeAuth.api_key, activeAuth.admin_password]);

  async function switchMode(mode: "lock" | "stage_aware") {
    try {
      await gatewayClient.setRouteMode(mode);
      onRouteModeChange(mode);
      onNotice(`Route mode hot-switched to ${mode}`);
    } catch (e) {
      onError(e, "Switch mode failed");
    }
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
    setScanningLocal(true);
    try {
      const customPorts = localCustomPorts
        .split(",")
        .map((s) => Number(s.trim()))
        .filter((n) => Number.isInteger(n) && n > 0 && n <= 65535);
      const found = await gatewayClient.scanLocalProviders(customPorts);
      setDiscoveredLocals(found);
      onNotice(
        found.length > 0
          ? `Discovered ${found.length} local inference service(s).`
          : "No local Ollama (11434) / LM Studio (1234) / vLLM (8000) services responded within 350ms."
      );
    } catch (e) {
      onError(e, "Local scan failed");
    } finally {
      setScanningLocal(false);
    }
  }

  async function handleImportLocalProvider(provider: DiscoveredLocalProvider) {
    try {
      const ch = await gatewayClient.importLocalProvider(
        provider,
        null,
        importAsOfflineFallback
      );
      await onRefresh();
      onNotice(
        `Imported '${ch.id}' (${ch.models.length} model(s))${
          ch.offline_fallback ? " as Offline Fallback" : ""
        } and persisted to config.local.toml.`
      );
    } catch (e) {
      onError(e, "Import local provider failed");
    }
  }

  async function handleSaveChannel() {
    try {
      const payload = buildChannelFromForm();
      const saved = await gatewayClient.upsertChannel(payload);
      setShowChannelForm(false);
      await onRefresh();
      onNotice(
        `Saved channel '${saved.id}' (${saved.base_url}) and persisted atomically to config.local.toml.`
      );
    } catch (e) {
      onError(e, "Save channel failed");
    }
  }

  async function handleDeleteChannel(id: string) {
    try {
      await gatewayClient.deleteChannel(id);
      await onRefresh();
      onNotice(`Removed channel '${id}' from live pool and config.local.toml.`);
    } catch (e) {
      onError(e, "Delete channel failed");
    }
  }

  async function handleToggleChannel(id: string, enabled: boolean) {
    try {
      await gatewayClient.toggleChannel(id, enabled);
      await onRefresh();
      onNotice(`Channel '${id}' ${enabled ? "enabled" : "disabled"}.`);
    } catch (e) {
      onError(e, "Toggle channel failed");
    }
  }

  async function handleTestChannel(cfg: ChannelConfig) {
    setTestingId(cfg.id || "__draft__");
    try {
      const res = await gatewayClient.testChannelConnection(cfg);
      setTestResults((prev) => ({ ...prev, [cfg.id || "__draft__"]: res }));
      onNotice(
        `Probe '${cfg.id || "draft"}': ${res.ok ? "OK" : "FAIL"} (${res.latency_ms} ms) — ${res.message}`
      );
    } catch (e) {
      onError(e, "Probe failed");
    } finally {
      setTestingId(null);
    }
  }

  async function toggleAutostart() {
    try {
      const next = await gatewayClient.setAutostartEnabled(!autostart);
      onAutostartChange(next);
      onNotice(next ? "Launch at login enabled (Windows Run key)." : "Launch at login disabled.");
    } catch (e) {
      onError(e, "Autostart update failed");
    }
  }

  async function saveAuthSettings() {
    try {
      const saved = await gatewayClient.setAuthSettings(
        apiKeyInput.trim() || null,
        adminPasswordInput.trim() || null
      );
      onActiveAuthChange(saved);
      setApiKeyInput(saved.api_key ?? "");
      setAdminPasswordInput(saved.admin_password ?? "");
      await onRefresh();
      onNotice(
        `Auth settings saved to config.local.toml (API Key: ${
          saved.api_key ? "Protected" : "Open"
        }, Admin Password: ${saved.admin_password ? "Protected" : "Open"}).`
      );
    } catch (e) {
      onError(e, "Save auth settings failed");
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
    try {
      const st = await gatewayClient.cloudLogin(cloudUrl, cloudEmail, cloudPassword);
      onCloudStatusChange(st);
      await onRefresh();
      onNotice(
        `Connected to Cloud Control Plane (${st.tenant_id}, ${st.synced_channels} team channels synced).`
      );
    } catch (e) {
      onError(e, "Cloud login failed");
    }
  }

  async function handleCloudSync() {
    try {
      const st = await gatewayClient.cloudSync();
      onCloudStatusChange(st);
      await onRefresh();
      onNotice(`Synced ${st.synced_channels} team channels from Cloud Control Plane.`);
    } catch (e) {
      onError(e, "Cloud sync failed");
    }
  }

  async function handleCloudLogout() {
    try {
      const st = await gatewayClient.cloudLogout();
      onCloudStatusChange(st);
      onNotice("Disconnected from Cloud Control Plane (running in local-only privacy mode).");
    } catch (e) {
      onError(e, "Cloud logout failed");
    }
  }

  return (
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
                      protocol: e.target.value as ChannelConfig["protocol"],
                    })
                  }
                >
                  <option value="anthropic">Anthropic (/v1/messages)</option>
                  <option value="openai">OpenAI (/v1/chat/completions)</option>
                  <option value="openai_responses">OpenAI Responses (/v1/responses)</option>
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
                      <div
                        style={{
                          fontSize: 11,
                          color: probe.ok ? "var(--ok)" : "var(--err)",
                          marginTop: 2,
                        }}
                      >
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
              {isDesktop()
                ? "Registers current executable under HKCU\\Software\\Microsoft\\CurrentVersion\\Run."
                : "Available in LoomGate Desktop app (not applicable in Docker / Web Console mode)."}
            </div>
          </div>
          <button
            className={`btn ${autostart ? "" : "secondary"}`}
            disabled={!isDesktop()}
            onClick={toggleAutostart}
          >
            {!isDesktop() ? "Desktop Only" : autostart ? "Enabled" : "Disabled"}
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
  );
}
