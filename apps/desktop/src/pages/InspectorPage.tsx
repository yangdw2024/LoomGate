import { useEffect, useState } from "react";
import { TrafficInspectEntry, gatewayClient } from "../api/client";

type InspectorSubTab = "ir" | "inbound" | "upstream" | "sse";

export interface InspectorPageProps {
  bindAddr: string;
  entries: TrafficInspectEntry[];
  onRefresh: () => Promise<void>;
  onNotice: (msg: string) => void;
  onError: (err: unknown, prefix?: string) => void;
}

function statusPillTone(status: string): "ok" | "warn" | "err" | "" {
  const s = status.toLowerCase();
  if (s === "completed" || s === "200" || s === "ok") return "ok";
  if (s === "guardrail_blocked" || s.startsWith("rejected_") || s.includes("error") || s.includes("fail")) {
    return "err";
  }
  if (s === "aborted_by_client" || s.includes("abort") || s.includes("timeout")) return "warn";
  return "";
}

function kvHitPct(entry: TrafficInspectEntry): number {
  const totalIn = entry.input_tokens + entry.cache_read_tokens;
  return totalIn > 0 ? Math.round((entry.cache_read_tokens / totalIn) * 100) : 0;
}

function formatPreview(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed) return "(empty payload)";
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      return JSON.stringify(JSON.parse(trimmed), null, 2);
    } catch {
      return raw;
    }
  }
  return raw;
}

function formatTime(tsMs: number): string {
  if (!tsMs) return "—";
  try {
    return new Date(tsMs).toLocaleTimeString();
  } catch {
    return String(tsMs);
  }
}

export default function InspectorPage({
  bindAddr,
  entries,
  onRefresh,
  onNotice,
  onError,
}: InspectorPageProps) {
  const [filterText, setFilterText] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [selectedDetail, setSelectedDetail] = useState<TrafficInspectEntry | null>(null);
  const [subTab, setSubTab] = useState<InspectorSubTab>("ir");
  const [clearing, setClearing] = useState(false);

  const normalizedFilter = filterText.trim().toLowerCase();
  const filteredEntries = normalizedFilter
    ? entries.filter((e) => {
        const haystack = [
          e.request_id,
          e.status,
          e.downstream_protocol,
          e.upstream_protocol,
          e.requested_model,
          e.actual_model,
          e.channel_id,
          e.route_mode,
          e.blocked_reason ?? "",
        ]
          .join(" ")
          .toLowerCase();
        return haystack.includes(normalizedFilter);
      })
    : entries;

  useEffect(() => {
    if (entries.length === 0) {
      setSelectedId(null);
      setSelectedDetail(null);
      return;
    }
    if (!selectedId && filteredEntries.length > 0) {
      setSelectedId(filteredEntries[0].request_id);
      setSelectedDetail(filteredEntries[0]);
      return;
    }
    if (selectedId) {
      const updated = entries.find((e) => e.request_id === selectedId);
      if (updated) {
        setSelectedDetail(updated);
      }
    }
  }, [entries, selectedId]);

  async function handleSelectEntry(entry: TrafficInspectEntry) {
    setSelectedId(entry.request_id);
    setSelectedDetail(entry);
    try {
      const full = await gatewayClient.getTrafficInspectorEntry(entry.request_id);
      if (full) {
        setSelectedDetail(full);
      }
    } catch (e) {
      onError(e, "Failed to load traffic entry");
    }
  }

  async function handleClear() {
    setClearing(true);
    try {
      await gatewayClient.clearTrafficInspector();
      setSelectedId(null);
      setSelectedDetail(null);
      await onRefresh();
      onNotice("Cleared Live Traffic Inspector ring buffer.");
    } catch (e) {
      onError(e, "Clear traffic inspector failed");
    } finally {
      setClearing(false);
    }
  }

  async function handleCopy(label: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      onNotice(`Copied ${label} to clipboard.`);
    } catch {
      onNotice(`Clipboard unavailable — select and copy ${label} manually.`);
    }
  }

  const activeEntry =
    selectedDetail ||
    (selectedId ? entries.find((e) => e.request_id === selectedId) ?? null : null);

  return (
    <div className="grid-2" style={{ gridTemplateColumns: "380px 1fr" }}>
      <div className="card" style={{ display: "flex", flexDirection: "column", overflow: "hidden", marginBottom: 0 }}>
        <div className="row" style={{ marginBottom: 10 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <h3 style={{ margin: 0 }}>Live Traffic Inspector</h3>
            <span className="pill">{filteredEntries.length}</span>
          </div>
          <button
            className="btn secondary"
            disabled={clearing || entries.length === 0}
            onClick={handleClear}
          >
            {clearing ? "Clearing..." : "Clear"}
          </button>
        </div>

        <input
          style={{ width: "100%", marginBottom: 10 }}
          value={filterText}
          onChange={(e) => setFilterText(e.target.value)}
          placeholder="Filter by protocol, status, model, channel..."
        />

        <div style={{ flex: 1, overflow: "auto" }}>
          {filteredEntries.map((entry) => {
            const hit = kvHitPct(entry);
            const tone = statusPillTone(entry.status);
            const isSelected = activeEntry?.request_id === entry.request_id;
            return (
              <div
                key={entry.request_id}
                className={`session-item ${isSelected ? "selected" : ""}`}
                onClick={() => handleSelectEntry(entry)}
              >
                <div className="row" style={{ marginBottom: 4 }}>
                  <span className={`pill ${tone}`}>{entry.status}</span>
                  <span style={{ fontSize: 11, color: "var(--muted)" }}>
                    {formatTime(entry.ts_ms)}
                  </span>
                </div>

                <div
                  style={{
                    fontFamily: "ui-monospace, Consolas, monospace",
                    fontSize: 12,
                    fontWeight: 600,
                    marginBottom: 4,
                    wordBreak: "break-all",
                  }}
                >
                  {entry.actual_model || entry.requested_model}
                </div>

                <div style={{ fontSize: 12, color: "var(--muted)", marginBottom: 6 }}>
                  {entry.downstream_protocol} → {entry.upstream_protocol} · {entry.channel_id}
                </div>

                <div style={{ display: "flex", flexWrap: "wrap", gap: 4 }}>
                  <span className="pill">{entry.ttft_ms} ms</span>
                  <span className={`pill ${hit > 0 ? "ok" : ""}`}>KV {hit}%</span>
                  {entry.sticky_hit && <span className="pill ok">sticky</span>}
                  {entry.guardrail_redactions > 0 && (
                    <span className="pill warn">
                      redactions: {entry.guardrail_redactions}
                    </span>
                  )}
                </div>
              </div>
            );
          })}

          {filteredEntries.length === 0 && (
            <p style={{ color: "var(--muted)", fontSize: 13 }}>
              {entries.length === 0
                ? `No live traffic captured yet. Send agent requests through ${bindAddr} to inspect wire frames, Canonical IR, and SSE timelines in real time.`
                : "No captured requests match the current filter."}
            </p>
          )}
        </div>
      </div>

      <div className="card" style={{ overflow: "auto", marginBottom: 0 }}>
        {!activeEntry ? (
          <div style={{ color: "var(--muted)", padding: "24px 8px" }}>
            <h3 style={{ marginBottom: 8 }}>Charles for AI Agents — Live Wire &amp; IR Inspector</h3>
            <p style={{ margin: 0 }}>
              Select a captured request on the left to inspect its Guardrail-redacted inbound wire
              payload, translated Canonical IR summary, upstream wire payload, and ordered SSE event
              timeline.
            </p>
          </div>
        ) : (
          <>
            <div className="row" style={{ flexWrap: "wrap", marginBottom: 10 }}>
              <div>
                <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                  <strong style={{ fontFamily: "ui-monospace, Consolas, monospace", fontSize: 14 }}>
                    {activeEntry.actual_model || activeEntry.requested_model}
                  </strong>
                  <span className={`pill ${statusPillTone(activeEntry.status)}`}>
                    {activeEntry.status}
                  </span>
                  <span className="pill">
                    {activeEntry.downstream_protocol} → {activeEntry.upstream_protocol}
                  </span>
                  <span className="pill">{activeEntry.stream ? "SSE Stream" : "Non-Stream"}</span>
                  {activeEntry.sticky_hit && <span className="pill ok">Sticky Hit</span>}
                  {activeEntry.guardrail_redactions > 0 && (
                    <span className="pill warn">
                      Guardrail Redactions: {activeEntry.guardrail_redactions}
                    </span>
                  )}
                </div>
                <div
                  style={{
                    color: "var(--muted)",
                    fontSize: 12,
                    marginTop: 4,
                    fontFamily: "ui-monospace, Consolas, monospace",
                  }}
                >
                  req: {activeEntry.request_id} · channel: {activeEntry.channel_id} · route:{" "}
                  {activeEntry.route_mode}
                  {activeEntry.requested_model &&
                  activeEntry.requested_model !== activeEntry.actual_model
                    ? ` · requested: ${activeEntry.requested_model}`
                    : ""}
                </div>
              </div>
              <span style={{ fontSize: 12, color: "var(--muted)" }}>
                {formatTime(activeEntry.ts_ms)}
              </span>
            </div>

            {activeEntry.blocked_reason && (
              <div className="alert-err">
                <strong>Blocked by Guardrail / Policy:</strong> {activeEntry.blocked_reason}
              </div>
            )}

            <div className="kpi-grid" style={{ marginBottom: 12 }}>
              <div className="kpi-card" style={{ padding: 10 }}>
                <div className="kpi-label">TTFT</div>
                <div className="kpi-value" style={{ fontSize: 18 }}>
                  {activeEntry.ttft_ms} ms
                </div>
              </div>
              <div className="kpi-card" style={{ padding: 10 }}>
                <div className="kpi-label">KV Cache Hit</div>
                <div className="kpi-value" style={{ fontSize: 18, color: "var(--ok)" }}>
                  {kvHitPct(activeEntry)}%
                </div>
                <div className="kpi-sub">
                  {activeEntry.cache_read_tokens.toLocaleString()} cached
                </div>
              </div>
              <div className="kpi-card" style={{ padding: 10 }}>
                <div className="kpi-label">Input Tokens</div>
                <div className="kpi-value" style={{ fontSize: 18 }}>
                  {activeEntry.input_tokens.toLocaleString()}
                </div>
              </div>
              <div className="kpi-card" style={{ padding: 10 }}>
                <div className="kpi-label">Output Tokens</div>
                <div className="kpi-value" style={{ fontSize: 18 }}>
                  {activeEntry.output_tokens.toLocaleString()}
                </div>
              </div>
            </div>

            <div className="row" style={{ marginBottom: 10, flexWrap: "wrap" }}>
              <div className="tabs" style={{ flexWrap: "wrap" }}>
                <button
                  className={`tab ${subTab === "ir" ? "active" : ""}`}
                  onClick={() => setSubTab("ir")}
                >
                  Canonical IR Summary
                </button>
                <button
                  className={`tab ${subTab === "inbound" ? "active" : ""}`}
                  onClick={() => setSubTab("inbound")}
                >
                  Inbound Wire (Redacted)
                </button>
                <button
                  className={`tab ${subTab === "upstream" ? "active" : ""}`}
                  onClick={() => setSubTab("upstream")}
                >
                  Upstream Wire (Redacted)
                </button>
                <button
                  className={`tab ${subTab === "sse" ? "active" : ""}`}
                  onClick={() => setSubTab("sse")}
                >
                  SSE Event Timeline ({activeEntry.sse_events.length})
                </button>
              </div>

              {subTab !== "sse" && (
                <button
                  className="btn secondary"
                  onClick={() =>
                    handleCopy(
                      subTab === "ir"
                        ? "Canonical IR Summary"
                        : subTab === "inbound"
                        ? "Inbound Wire"
                        : "Upstream Wire",
                      subTab === "ir"
                        ? activeEntry.canonical_ir_summary
                        : subTab === "inbound"
                        ? activeEntry.inbound_preview
                        : activeEntry.upstream_preview
                    )
                  }
                >
                  Copy
                </button>
              )}
            </div>

            {subTab === "ir" && (
              <pre className="inspector-pre">
                {formatPreview(activeEntry.canonical_ir_summary)}
              </pre>
            )}

            {subTab === "inbound" && (
              <pre className="inspector-pre">{formatPreview(activeEntry.inbound_preview)}</pre>
            )}

            {subTab === "upstream" && (
              <pre className="inspector-pre">{formatPreview(activeEntry.upstream_preview)}</pre>
            )}

            {subTab === "sse" && (
              <div className="sse-timeline">
                {activeEntry.sse_events.length > 0 ? (
                  activeEntry.sse_events.map((ev, idx) => (
                    <span key={`${idx}-${ev}`} className="sse-pill">
                      <span className="sse-pill-idx">#{idx + 1}</span>
                      <span>{ev}</span>
                    </span>
                  ))
                ) : (
                  <span style={{ color: "var(--muted)", fontSize: 13 }}>
                    {activeEntry.stream
                      ? "No SSE events recorded for this request."
                      : "Non-streaming request (no SSE events)."}
                  </span>
                )}
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
