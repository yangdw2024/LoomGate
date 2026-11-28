import { useState } from "react";
import { ClientProfileStatus, gatewayClient } from "../api/client";

export interface ClientsPageProps {
  profiles: ClientProfileStatus[];
  onRefresh: () => Promise<void>;
  onNotice: (msg: string) => void;
  onError: (err: unknown, prefix?: string) => void;
}

export default function ClientsPage({
  profiles,
  onRefresh,
  onNotice,
  onError,
}: ClientsPageProps) {
  const [selectedWizardId, setSelectedWizardId] = useState<string>("cursor");

  async function handleTakeoverProfile(profileId: string) {
    try {
      await gatewayClient.takeoverClientProfile(profileId);
      await onRefresh();
      onNotice(`Atomically took over '${profileId}' config (backup created).`);
    } catch (e) {
      onError(e, "Takeover failed");
    }
  }

  async function handleRestoreProfile(profileId: string) {
    try {
      await gatewayClient.restoreClientProfile(profileId);
      await onRefresh();
      onNotice(`Restored original '${profileId}' config from backup.`);
    } catch (e) {
      onError(e, "Restore failed");
    }
  }

  async function copyText(label: string, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      onNotice(`Copied ${label} to clipboard.`);
    } catch {
      onNotice(`Clipboard unavailable — select and copy ${label} manually.`);
    }
  }

  const atomicProfiles = profiles.filter((p) => p.kind === "atomic_takeover");
  const wizardProfiles = profiles.filter((p) => p.kind === "setup_wizard");
  const activeWizard =
    wizardProfiles.find((p) => p.id === selectedWizardId) || wizardProfiles[0] || null;

  return (
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
  );
}
