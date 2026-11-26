import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface ChannelSnapshot {
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

export interface ChannelConfig {
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

export interface ChannelTestResult {
  ok: boolean;
  channel_id: string;
  status_code: number | null;
  latency_ms: number;
  endpoint: string;
  message: string;
  quota_info?: unknown;
}

export interface DiscoveredLocalProvider {
  kind: "ollama" | "lm_studio" | "vllm" | "custom_openai";
  display_name: string;
  suggested_channel_id: string;
  base_url: string;
  port: number;
  models: string[];
  latency_ms: number;
}

export interface ModelSavingsRow {
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

export interface SavingsSummary {
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

export interface AuthSettings {
  api_key: string | null;
  admin_password: string | null;
}

export interface ClientProfileStatus {
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

export interface SessionSummary {
  session_id: string;
  workspace: string;
  step_count: number;
  fork_count: number;
  total_cache_read_tokens: number;
  total_input_tokens: number;
  last_model: string;
  updated_at_ms: number;
}

export interface TrajectoryStep {
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

export interface ForkDiffResult {
  original: TrajectoryStep;
  forked: TrajectoryStep;
  ttft_delta_ms: number;
  output_tokens_delta: number;
  cache_hit_ratio_delta: number;
}

export interface ForkDiffExport {
  original_step_id: string;
  forked_step_id: string;
  original_model: string;
  forked_model: string;
  redacted: boolean;
  redactions_applied: number;
  sanitized_original_text: string;
  sanitized_forked_text: string;
  markdown: string;
}

export interface CloudSessionStatus {
  connected: boolean;
  control_plane_url: string | null;
  email: string | null;
  tenant_id: string | null;
  synced_channels: number;
  available_micros: number | null;
  telemetry_uploaded: number;
  last_sync_ms: number | null;
}

export interface GatewayStatus {
  bind_addr: string;
  route_mode: "lock" | "stage_aware";
  channels: ChannelSnapshot[];
}

const SESSION_ADMIN_PW_KEY = "loomgate_admin_password";

export class AdminAuthRequiredError extends Error {
  constructor(message = "Admin password required to access LoomGate Web Console.") {
    super(message);
    this.name = "AdminAuthRequiredError";
  }
}

export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Reads the Web Console admin password strictly from ephemeral browser sessionStorage (never localStorage). */
export function getSessionAdminPassword(): string {
  if (typeof window === "undefined" || !window.sessionStorage) return "";
  return window.sessionStorage.getItem(SESSION_ADMIN_PW_KEY) ?? "";
}

export function setSessionAdminPassword(password: string): void {
  if (typeof window === "undefined" || !window.sessionStorage) return;
  const trimmed = password.trim();
  if (trimmed) {
    window.sessionStorage.setItem(SESSION_ADMIN_PW_KEY, trimmed);
  } else {
    window.sessionStorage.removeItem(SESSION_ADMIN_PW_KEY);
  }
}

export function clearSessionAdminPassword(): void {
  if (typeof window === "undefined" || !window.sessionStorage) return;
  window.sessionStorage.removeItem(SESSION_ADMIN_PW_KEY);
}

async function httpJson<T>(
  method: "GET" | "POST" | "DELETE",
  path: string,
  body?: unknown
): Promise<T> {
  const headers: Record<string, string> = {
    Accept: "application/json",
  };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  const adminPw = getSessionAdminPassword();
  if (adminPw) {
    headers["x-admin-password"] = adminPw;
  }

  const res = await fetch(path, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });

  if (res.status === 401) {
    throw new AdminAuthRequiredError();
  }

  const text = await res.text();
  let parsed: unknown = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = { error: text };
  }

  if (!res.ok) {
    const errMsg =
      typeof parsed === "object" && parsed !== null && "error" in parsed
        ? String((parsed as { error: unknown }).error)
        : `HTTP ${res.status}`;
    throw new Error(errMsg);
  }

  return parsed as T;
}

/**
 * Unified Dual-Mode Client:
 * - Inside Tauri v2 Desktop -> native IPC `invoke(...)`
 * - Inside any Browser (Docker / remote daemon) -> same-origin HTTP REST API with sessionStorage `x-admin-password`
 */
export const gatewayClient = {
  isTauri: isTauriRuntime,

  async onRouteModeChanged(
    handler: (mode: "lock" | "stage_aware") => void
  ): Promise<() => void> {
    if (!isTauriRuntime()) {
      return () => {};
    }
    return await listen<"lock" | "stage_aware">("route-mode-changed", (event) => {
      handler(event.payload);
    });
  },

  async getGatewayStatus(): Promise<GatewayStatus> {
    if (isTauriRuntime()) {
      return await invoke<GatewayStatus>("get_gateway_status");
    }
    const [health, metrics] = await Promise.all([
      httpJson<{ bind_addr?: string; route_mode?: "lock" | "stage_aware" }>("GET", "/healthz"),
      httpJson<{ channels: ChannelSnapshot[] }>("GET", "/metrics/channels"),
    ]);
    return {
      bind_addr: health.bind_addr || window.location.host,
      route_mode: health.route_mode === "stage_aware" ? "stage_aware" : "lock",
      channels: metrics.channels || [],
    };
  },

  async setRouteMode(mode: "lock" | "stage_aware"): Promise<string> {
    if (isTauriRuntime()) {
      return await invoke<string>("set_route_mode", { mode });
    }
    const res = await httpJson<{ mode: string }>("POST", "/api/route-mode", { mode });
    return res.mode;
  },

  async listChannelConfigs(): Promise<ChannelConfig[]> {
    if (isTauriRuntime()) {
      return await invoke<ChannelConfig[]>("list_channel_configs");
    }
    const res = await httpJson<{ configs: ChannelConfig[] }>("GET", "/api/channels");
    return res.configs || [];
  },

  async upsertChannel(channel: ChannelConfig): Promise<ChannelSnapshot> {
    if (isTauriRuntime()) {
      return await invoke<ChannelSnapshot>("upsert_channel", { channel });
    }
    const res = await httpJson<{ channel: ChannelSnapshot }>("POST", "/api/channels", channel);
    return res.channel;
  },

  async deleteChannel(id: string): Promise<boolean> {
    if (isTauriRuntime()) {
      return await invoke<boolean>("delete_channel", { id });
    }
    const res = await httpJson<{ deleted: boolean }>(
      "DELETE",
      `/api/channels/${encodeURIComponent(id)}`
    );
    return Boolean(res.deleted);
  },

  async toggleChannel(id: string, enabled: boolean): Promise<ChannelSnapshot> {
    if (isTauriRuntime()) {
      return await invoke<ChannelSnapshot>("toggle_channel", { id, enabled });
    }
    const res = await httpJson<{ channel: ChannelSnapshot }>(
      "POST",
      `/api/channels/${encodeURIComponent(id)}/toggle`,
      { enabled }
    );
    return res.channel;
  },

  async testChannelConnection(channel: ChannelConfig): Promise<ChannelTestResult> {
    if (isTauriRuntime()) {
      return await invoke<ChannelTestResult>("test_channel_connection", { channel });
    }
    return await httpJson<ChannelTestResult>("POST", "/api/channels/test", channel);
  },

  async scanLocalProviders(customPorts: number[]): Promise<DiscoveredLocalProvider[]> {
    if (isTauriRuntime()) {
      return await invoke<DiscoveredLocalProvider[]>("scan_local_providers", { customPorts });
    }
    const q = customPorts.length > 0 ? `?ports=${encodeURIComponent(customPorts.join(","))}` : "";
    const res = await httpJson<{ providers: DiscoveredLocalProvider[] }>(
      "GET",
      `/api/local/discover${q}`
    );
    return res.providers || [];
  },

  async importLocalProvider(
    provider: DiscoveredLocalProvider,
    channelId: string | null,
    offlineFallback: boolean
  ): Promise<ChannelSnapshot> {
    if (isTauriRuntime()) {
      return await invoke<ChannelSnapshot>("import_local_provider", {
        provider,
        channelId,
        offlineFallback,
      });
    }
    const res = await httpJson<{ channel: ChannelSnapshot }>("POST", "/api/local/import", {
      provider,
      channel_id: channelId,
      offline_fallback: offlineFallback,
    });
    return res.channel;
  },

  async getSavingsSummary(days: number): Promise<SavingsSummary> {
    if (isTauriRuntime()) {
      return await invoke<SavingsSummary>("get_savings_summary", { days });
    }
    return await httpJson<SavingsSummary>(
      "GET",
      `/api/analytics/savings?days=${encodeURIComponent(String(days))}`
    );
  },

  async listClientProfiles(): Promise<ClientProfileStatus[]> {
    if (isTauriRuntime()) {
      return await invoke<ClientProfileStatus[]>("list_client_profiles");
    }
    const res = await httpJson<{ profiles: ClientProfileStatus[] }>("GET", "/api/clients");
    return res.profiles || [];
  },

  async takeoverClientProfile(profileId: string): Promise<ClientProfileStatus> {
    if (isTauriRuntime()) {
      return await invoke<ClientProfileStatus>("takeover_client_profile", { profileId });
    }
    const res = await httpJson<{ profile: ClientProfileStatus }>(
      "POST",
      `/api/clients/${encodeURIComponent(profileId)}/takeover`
    );
    return res.profile;
  },

  async restoreClientProfile(profileId: string): Promise<ClientProfileStatus> {
    if (isTauriRuntime()) {
      return await invoke<ClientProfileStatus>("restore_client_profile", { profileId });
    }
    const res = await httpJson<{ profile: ClientProfileStatus }>(
      "POST",
      `/api/clients/${encodeURIComponent(profileId)}/restore`
    );
    return res.profile;
  },

  async getAutostartEnabled(): Promise<boolean> {
    if (!isTauriRuntime()) return false;
    return await invoke<boolean>("get_autostart_enabled");
  },

  async setAutostartEnabled(enabled: boolean): Promise<boolean> {
    if (!isTauriRuntime()) {
      throw new Error("Launch at login is only available in the LoomGate Desktop app.");
    }
    return await invoke<boolean>("set_autostart_enabled", { enabled });
  },

  async getAuthSettings(): Promise<AuthSettings> {
    if (isTauriRuntime()) {
      return await invoke<AuthSettings>("get_auth_settings");
    }
    return await httpJson<AuthSettings>("GET", "/api/auth/settings");
  },

  async setAuthSettings(
    apiKey: string | null,
    adminPassword: string | null
  ): Promise<AuthSettings> {
    if (isTauriRuntime()) {
      return await invoke<AuthSettings>("set_auth_settings", { apiKey, adminPassword });
    }
    const res = await httpJson<{ auth: AuthSettings }>("POST", "/api/auth/settings", {
      api_key: apiKey,
      admin_password: adminPassword,
    });
    if (res.auth.admin_password) {
      setSessionAdminPassword(res.auth.admin_password);
    } else {
      clearSessionAdminPassword();
    }
    return res.auth;
  },

  async getCloudStatus(): Promise<CloudSessionStatus> {
    if (isTauriRuntime()) {
      return await invoke<CloudSessionStatus>("get_cloud_status");
    }
    return await httpJson<CloudSessionStatus>("GET", "/api/cloud/status");
  },

  async cloudLogin(
    controlPlaneUrl: string,
    email: string,
    password: string
  ): Promise<CloudSessionStatus> {
    if (isTauriRuntime()) {
      return await invoke<CloudSessionStatus>("cloud_login", {
        controlPlaneUrl,
        email,
        password,
      });
    }
    const res = await httpJson<{ cloud: CloudSessionStatus }>("POST", "/api/cloud/login", {
      control_plane_url: controlPlaneUrl,
      email,
      password,
    });
    return res.cloud;
  },

  async cloudSync(): Promise<CloudSessionStatus> {
    if (isTauriRuntime()) {
      return await invoke<CloudSessionStatus>("cloud_sync");
    }
    const res = await httpJson<{ cloud: CloudSessionStatus }>("POST", "/api/cloud/sync");
    return res.cloud;
  },

  async cloudLogout(): Promise<CloudSessionStatus> {
    if (isTauriRuntime()) {
      return await invoke<CloudSessionStatus>("cloud_logout");
    }
    const res = await httpJson<{ cloud: CloudSessionStatus }>("POST", "/api/cloud/logout");
    return res.cloud;
  },

  async listSessions(): Promise<SessionSummary[]> {
    if (isTauriRuntime()) {
      return await invoke<SessionSummary[]>("list_sessions");
    }
    const res = await httpJson<{ sessions: SessionSummary[] }>("GET", "/api/sessions");
    return res.sessions || [];
  },

  async getSessionSteps(sessionId: string): Promise<TrajectoryStep[]> {
    if (isTauriRuntime()) {
      return await invoke<TrajectoryStep[]>("get_session_steps", { sessionId });
    }
    const res = await httpJson<{ steps: TrajectoryStep[] }>(
      "GET",
      `/api/sessions/${encodeURIComponent(sessionId)}/steps`
    );
    return res.steps || [];
  },

  async forkAndDiffStep(stepId: string, targetModel: string): Promise<ForkDiffResult> {
    if (isTauriRuntime()) {
      return await invoke<ForkDiffResult>("fork_and_diff_step", { stepId, targetModel });
    }
    return await httpJson<ForkDiffResult>(
      "POST",
      `/api/steps/${encodeURIComponent(stepId)}/fork`,
      { target_model: targetModel }
    );
  },

  async exportForkDiffMarkdown(
    originalStepId: string,
    forkedStepId: string,
    redactSecrets: boolean
  ): Promise<ForkDiffExport> {
    if (isTauriRuntime()) {
      return await invoke<ForkDiffExport>("export_fork_diff_markdown", {
        originalStepId,
        forkedStepId,
        redactSecrets,
      });
    }
    return await httpJson<ForkDiffExport>("POST", "/api/steps/export-diff", {
      original_step_id: originalStepId,
      forked_step_id: forkedStepId,
      redact_secrets: redactSecrets,
    });
  },
};
