/**
 * Official Zero-Dependency SDK for LoomGate Edge-Cloud Hybrid Microkernel AI Gateway.
 */
export class LoomGateClient {
  constructor(options = {}) {
    this.baseUrl = (options.baseUrl || process.env.LOOMGATE_URL || "http://127.0.0.1:8787").replace(/\/+$/, "");
    this.token = options.token ?? process.env.LOOMGATE_TOKEN;
    this.fetchImpl = options.fetchImpl || globalThis.fetch;
    if (typeof this.fetchImpl !== "function") {
      throw new Error("Global fetch is unavailable; Node.js >= 18 is required or pass options.fetchImpl.");
    }
  }

  _buildHeaders(opts = {}) {
    const headers = {
      "Content-Type": "application/json",
      ...(opts.headers || {}),
    };
    if (this.token) {
      headers["Authorization"] = `Bearer ${this.token}`;
    }
    if (opts.sessionId) {
      headers["X-Session-Id"] = opts.sessionId;
    }
    if (opts.taskStage) {
      headers["X-Gateway-Task-Stage"] = opts.taskStage;
    }
    if (opts.routeMode) {
      headers["X-Gateway-Route-Mode"] = opts.routeMode;
    }
    return headers;
  }

  async _request(method, path, body, opts = {}) {
    const res = await this.fetchImpl(`${this.baseUrl}${path}`, {
      method,
      headers: this._buildHeaders(opts),
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    let data;
    try {
      data = text ? JSON.parse(text) : null;
    } catch {
      data = { raw: text };
    }
    const headers = {};
    res.headers.forEach((v, k) => {
      headers[k.toLowerCase()] = v;
    });
    return { status: res.status, headers, data };
  }

  async _mustOk(method, path, body, opts = {}) {
    const r = await this._request(method, path, body, opts);
    if (r.status < 200 || r.status >= 300) {
      const msg = typeof r.data === "object" && r.data !== null ? JSON.stringify(r.data) : String(r.data);
      throw new Error(`LoomGate HTTP ${r.status} on ${method} ${path}: ${msg}`);
    }
    return r.data;
  }

  health() {
    return this._mustOk("GET", "/healthz");
  }

  async channels() {
    const res = await this._mustOk("GET", "/metrics/channels");
    return res.channels || [];
  }

  middleware() {
    return this._mustOk("GET", "/api/middleware");
  }

  async sessions() {
    const res = await this._mustOk("GET", "/api/sessions");
    return res.sessions || [];
  }

  async sessionSteps(sessionId) {
    const res = await this._mustOk("GET", `/api/sessions/${encodeURIComponent(sessionId)}/steps`);
    return res.steps || [];
  }

  forkStep(stepId, targetModel) {
    return this._mustOk("POST", `/api/steps/${encodeURIComponent(stepId)}/fork`, {
      target_model: targetModel,
    });
  }

  callMcpTool(tool, args = {}) {
    return this._mustOk("POST", "/api/mcp/call", { tool, arguments: args });
  }

  cloudStatus() {
    return this._mustOk("GET", "/api/cloud/status");
  }

  async cloudLogin(controlPlaneUrl, email, password) {
    const res = await this._mustOk("POST", "/api/cloud/login", {
      control_plane_url: controlPlaneUrl,
      email,
      password,
    });
    return res.cloud;
  }

  async cloudSync() {
    const res = await this._mustOk("POST", "/api/cloud/sync");
    return res.cloud;
  }

  async cloudLogout() {
    const res = await this._mustOk("POST", "/api/cloud/logout");
    return res.cloud;
  }

  messages(payload, opts = {}) {
    return this._request("POST", "/v1/messages", payload, opts);
  }

  chatCompletions(payload, opts = {}) {
    return this._request("POST", "/v1/chat/completions", payload, opts);
  }

  generateContent(model, payload, opts = {}) {
    return this._request(
      "POST",
      `/v1beta/models/${encodeURIComponent(model)}:generateContent`,
      payload,
      opts
    );
  }
}

export default LoomGateClient;
