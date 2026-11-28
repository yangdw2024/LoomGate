import { useState } from "react";
import {
  ForkDiffExport,
  ForkDiffResult,
  SessionSummary,
  TrajectoryStep,
  gatewayClient,
} from "../api/client";
import {
  buildShareCardSvg,
  computeLineDiff,
  renderShareCardPngDataUrl,
  triggerDownload,
} from "../utils/diff";

export interface TrajectoryPageProps {
  bindAddr: string;
  sessions: SessionSummary[];
  selectedSession: string | null;
  steps: TrajectoryStep[];
  onSelectSession: (sessionId: string) => void;
  onStepsChange: (steps: TrajectoryStep[]) => void;
  onNotice: (msg: string) => void;
  onError: (err: unknown, prefix?: string) => void;
}

export default function TrajectoryPage({
  bindAddr,
  sessions,
  selectedSession,
  steps,
  onSelectSession,
  onStepsChange,
  onNotice,
  onError,
}: TrajectoryPageProps) {
  const [forkModel, setForkModel] = useState("gpt-4o");
  const [forkingStep, setForkingStep] = useState<string | null>(null);
  const [diffResult, setDiffResult] = useState<ForkDiffResult | null>(null);
  const [diffViewMode, setDiffViewMode] = useState<"unified" | "split">("unified");
  const [redactOnExport, setRedactOnExport] = useState<boolean>(true);

  async function copyText(label: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      onNotice(`Copied ${label} to clipboard.`);
    } catch {
      onNotice(`Clipboard unavailable — select and copy ${label} manually.`);
    }
  }

  async function openSession(sessionId: string) {
    onSelectSession(sessionId);
    setDiffResult(null);
    try {
      const st = await gatewayClient.getSessionSteps(sessionId);
      onStepsChange(st);
    } catch (e) {
      onError(e, "Load session failed");
    }
  }

  async function handleFork(stepId: string) {
    setForkingStep(stepId);
    try {
      const res = await gatewayClient.forkAndDiffStep(stepId, forkModel);
      setDiffResult(res);
      if (selectedSession) {
        const st = await gatewayClient.getSessionSteps(selectedSession);
        onStepsChange(st);
      }
    } catch (e) {
      onError(e, "Fork failed");
    } finally {
      setForkingStep(null);
    }
  }

  async function fetchExportData(): Promise<ForkDiffExport | null> {
    if (!diffResult) return null;
    return await gatewayClient.exportForkDiffMarkdown(
      diffResult.original.step_id,
      diffResult.forked.step_id,
      redactOnExport
    );
  }

  async function handleCopyMarkdownReport() {
    try {
      const exp = await fetchExportData();
      if (!exp) return;
      await copyText("Markdown Fork & Diff Report", exp.markdown);
      onNotice(
        `Copied Markdown report (${
          exp.redacted
            ? `Guardrail redacted ${exp.redactions_applied} secret/PII item(s)`
            : "raw unredacted"
        }).`
      );
    } catch (e) {
      onNotice(`Export Markdown failed: ${String(e)}`);
    }
  }

  async function handleExportSvgCard() {
    if (!diffResult) return;
    try {
      const exp = await fetchExportData();
      if (!exp) return;
      const svg = buildShareCardSvg(
        diffResult,
        exp.sanitized_original_text,
        exp.sanitized_forked_text,
        exp.redacted,
        exp.redactions_applied
      );
      const blob = new Blob([svg], { type: "image/svg+xml;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      triggerDownload(
        `loomgate-diff-${diffResult.original.actual_model}-vs-${diffResult.forked.actual_model}.svg`,
        url
      );
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      onNotice(
        `Exported SVG share card (${
          exp.redacted ? `${exp.redactions_applied} redaction(s) applied` : "raw"
        }).`
      );
    } catch (e) {
      onNotice(`Export SVG failed: ${String(e)}`);
    }
  }

  async function handleExportPngCard() {
    if (!diffResult) return;
    try {
      const exp = await fetchExportData();
      if (!exp) return;
      const dataUrl = renderShareCardPngDataUrl(
        diffResult,
        exp.sanitized_original_text,
        exp.sanitized_forked_text,
        exp.redacted,
        exp.redactions_applied
      );
      triggerDownload(
        `loomgate-diff-${diffResult.original.actual_model}-vs-${diffResult.forked.actual_model}.png`,
        dataUrl
      );
      onNotice(
        `Exported PNG share card via pure Canvas 2D (${
          exp.redacted ? `${exp.redactions_applied} redaction(s) applied` : "raw"
        }).`
      );
    } catch (e) {
      onNotice(`Export PNG failed: ${String(e)}`);
    }
  }

  return (
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
              className={`session-item ${selectedSession === s.session_id ? "selected" : ""}`}
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
            <div className="row" style={{ marginTop: 10, flexWrap: "wrap" }}>
              <label style={{ display: "flex", alignItems: "center", gap: 6, fontSize: 12 }}>
                <input
                  type="checkbox"
                  checked={redactOnExport}
                  onChange={(e) => setRedactOnExport(e.target.checked)}
                />
                Redact Secrets &amp; PII before export (Guardrail)
              </label>
              <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                <button className="btn secondary" onClick={handleCopyMarkdownReport}>
                  Copy Markdown Report
                </button>
                <button className="btn secondary" onClick={handleExportSvgCard}>
                  Export SVG Card
                </button>
                <button className="btn" onClick={handleExportPngCard}>
                  Export PNG Card
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
  );
}
