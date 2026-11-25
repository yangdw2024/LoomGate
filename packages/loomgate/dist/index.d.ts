export interface LoomGateClientOptions {
  /** Base URL of the local LoomGate daemon or Tauri embedded proxy (default: http://127.0.0.1:8787). */
  baseUrl?: string;
  /** Optional local inbound auth token (sent as Authorization: Bearer / x-api-key). */
  token?: string;
  /** Optional custom fetch implementation. */
  fetchImpl?: typeof fetch;
}

export interface RequestMetaOptions {
  sessionId?: string;
  taskStage?: string;
  routeMode?: "lock" | "stage_aware";
  headers?: Record<string, string>;
}

export interface GatewayHealth {
  status: string;
  version?: string;
  kernel?: string;
  embedded?: string;
  route_mode: "lock" | "stage_aware";
}

export interface ChannelSnapshot {
  id: string;
  protocol: string;
  health: "healthy" | "cooldown" | "quarantined";
  inflight: number;
  ewma_ttft_ms: number | null;
  total_requests: number;
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

export interface MiddlewareOverview {
  layers: string[];
  mcp_tools: Array<{
    name: string;
    description?: string;
    parameters: Record<string, unknown>;
    strict: boolean;
  }>;
}

export interface GatewayCallResult<T = unknown> {
  status: number;
  headers: Record<string, string>;
  data: T;
}

export class LoomGateClient {
  readonly baseUrl: string;
  readonly token?: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options?: LoomGateClientOptions);

  health(): Promise<GatewayHealth>;
  channels(): Promise<ChannelSnapshot[]>;
  middleware(): Promise<MiddlewareOverview>;
  sessions(): Promise<SessionSummary[]>;
  sessionSteps(sessionId: string): Promise<TrajectoryStep[]>;
  forkStep(stepId: string, targetModel: string): Promise<ForkDiffResult>;
  callMcpTool(tool: string, args?: Record<string, unknown>): Promise<{ ok: boolean; result?: unknown; error?: string }>;

  cloudStatus(): Promise<CloudSessionStatus>;
  cloudLogin(controlPlaneUrl: string, email: string, password: string): Promise<CloudSessionStatus>;
  cloudSync(): Promise<CloudSessionStatus>;
  cloudLogout(): Promise<CloudSessionStatus>;

  /** Anthropic Messages API (/v1/messages) routed through LoomGate lossless Canonical IR. */
  messages<T = Record<string, unknown>>(payload: Record<string, unknown>, opts?: RequestMetaOptions): Promise<GatewayCallResult<T>>;
  /** OpenAI Chat Completions API (/v1/chat/completions) routed through LoomGate lossless Canonical IR. */
  chatCompletions<T = Record<string, unknown>>(payload: Record<string, unknown>, opts?: RequestMetaOptions): Promise<GatewayCallResult<T>>;
  /** Gemini Native API (/v1beta/models/<model>:generateContent) routed through LoomGate lossless Canonical IR. */
  generateContent<T = Record<string, unknown>>(model: string, payload: Record<string, unknown>, opts?: RequestMetaOptions): Promise<GatewayCallResult<T>>;
}
