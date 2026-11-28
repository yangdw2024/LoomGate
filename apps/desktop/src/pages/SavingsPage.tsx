import { SavingsSummary } from "../api/client";

export interface SavingsPageProps {
  bindAddr: string;
  savings: SavingsSummary | null;
  savingsDays: number;
  onSavingsDaysChange: (days: number) => void;
}

export default function SavingsPage({
  bindAddr,
  savings,
  savingsDays,
  onSavingsDaysChange,
}: SavingsPageProps) {
  return (
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
              onClick={() => onSavingsDaysChange(w.days)}
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
  );
}
