import { useEffect, useState } from "react";
import {
  AdminAuthRequiredError,
  AuthSettings,
  ChannelConfig,
  ChannelSnapshot,
  ClientProfileStatus,
  CloudSessionStatus,
  SavingsSummary,
  SessionSummary,
  TrafficInspectEntry,
  TrajectoryStep,
  clearSessionAdminPassword,
  gatewayClient,
  getSessionAdminPassword,
  setSessionAdminPassword,
} from "./api/client";
import ChannelsPage from "./pages/ChannelsPage";
import ClientsPage from "./pages/ClientsPage";
import InspectorPage from "./pages/InspectorPage";
import SavingsPage from "./pages/SavingsPage";
import TrajectoryPage from "./pages/TrajectoryPage";

type TabId = "channels" | "inspector" | "takeover" | "trajectory" | "savings";

const isDesktop = () => gatewayClient.isTauri();

export default function App() {
  const [tab, setTab] = useState<TabId>("channels");
  const [bindAddr, setBindAddr] = useState("127.0.0.1:8787");
  const [routeMode, setRouteMode] = useState<"lock" | "stage_aware">("lock");
  const [channels, setChannels] = useState<ChannelSnapshot[]>([]);
  const [channelConfigs, setChannelConfigs] = useState<ChannelConfig[]>([]);
  const [trafficEntries, setTrafficEntries] = useState<TrafficInspectEntry[]>([]);

  const [savingsDays, setSavingsDays] = useState<number>(30);
  const [savings, setSavings] = useState<SavingsSummary | null>(null);
  const [profiles, setProfiles] = useState<ClientProfileStatus[]>([]);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [steps, setSteps] = useState<TrajectoryStep[]>([]);

  const [autostart, setAutostart] = useState<boolean>(false);
  const [activeAuth, setActiveAuth] = useState<AuthSettings>({
    api_key: null,
    admin_password: null,
  });
  const [cloudStatus, setCloudStatus] = useState<CloudSessionStatus | null>(null);

  const [adminAuthLocked, setAdminAuthLocked] = useState<boolean>(false);
  const [webAdminPrompt, setWebAdminPrompt] = useState<string>("");
  const [notice, setNotice] = useState<string>("");

  function handleApiError(e: unknown, prefix?: string) {
    if (e instanceof AdminAuthRequiredError) {
      setAdminAuthLocked(true);
      setNotice("Web Console requires Admin Password (stored only in browser sessionStorage).");
      return;
    }
    setNotice(prefix ? `${prefix}: ${String(e)}` : String(e));
  }

  async function refreshAll() {
    try {
      const status = await gatewayClient.getGatewayStatus();
      setBindAddr(status.bind_addr);
      setRouteMode(status.route_mode);
      setChannels(status.channels);

      const [cfgList, profList, cloud, sessList, sav, traffic] = await Promise.all([
        gatewayClient.listChannelConfigs(),
        gatewayClient.listClientProfiles(),
        gatewayClient.getCloudStatus(),
        gatewayClient.listSessions(),
        gatewayClient.getSavingsSummary(savingsDays),
        gatewayClient.listTrafficInspector(50),
      ]);
      setAdminAuthLocked(false);
      setChannelConfigs(cfgList);
      setProfiles(profList);
      setCloudStatus(cloud);
      setSessions(sessList);
      setSavings(sav);
      setTrafficEntries(traffic);
      if (selectedSession) {
        const st = await gatewayClient.getSessionSteps(selectedSession);
        setSteps(st);
      }
    } catch (e) {
      if (e instanceof AdminAuthRequiredError) {
        setAdminAuthLocked(true);
        return;
      }
      throw e;
    }
  }

  useEffect(() => {
    gatewayClient
      .getAutostartEnabled()
      .then(setAutostart)
      .catch(() => {});
    gatewayClient
      .getAuthSettings()
      .then(setActiveAuth)
      .catch((e) => {
        if (e instanceof AdminAuthRequiredError) {
          setAdminAuthLocked(true);
        }
      });
  }, []);

  useEffect(() => {
    refreshAll().catch((e) => handleApiError(e));
    const timer = setInterval(() => {
      refreshAll().catch(() => {});
    }, 2000);
    let unlisten: (() => void) | undefined;
    gatewayClient
      .onRouteModeChanged((mode) => {
        setRouteMode(mode);
        setNotice(`Tray hot-switched route mode to ${mode}`);
      })
      .then((fn) => {
        unlisten = fn;
      })
      .catch(() => {});
    return () => {
      clearInterval(timer);
      if (unlisten) unlisten();
    };
  }, [selectedSession, savingsDays]);

  async function handleUnlockWebAdmin() {
    setSessionAdminPassword(webAdminPrompt);
    setWebAdminPrompt("");
    try {
      await refreshAll();
      const auth = await gatewayClient.getAuthSettings();
      setActiveAuth(auth);
      setAdminAuthLocked(false);
      setNotice("Web Console unlocked (sessionStorage only).");
    } catch (e) {
      clearSessionAdminPassword();
      handleApiError(e, "Admin unlock failed");
    }
  }

  function handleLockWebAdmin() {
    clearSessionAdminPassword();
    setAdminAuthLocked(true);
    setNotice("Admin session cleared from sessionStorage.");
  }

  return (
    <div className="layout">
      <header className="topbar">
        <div className="brand">
          <span className={`status-dot ${routeMode === "lock" ? "lock" : "stage"}`} />
          <span>LoomGate</span>
          <span className="pill ok">{bindAddr}</span>
          <span className="pill">{routeMode === "lock" ? "Lock Mode" : "Stage-Aware"}</span>
          <span className="pill">{isDesktop() ? "Desktop" : "Web Console"}</span>
        </div>
        <div className="tabs">
          <button
            className={`tab ${tab === "channels" ? "active" : ""}`}
            onClick={() => setTab("channels")}
          >
            Channels &amp; Routing
          </button>
          <button
            className={`tab ${tab === "inspector" ? "active" : ""}`}
            onClick={() => setTab("inspector")}
          >
            Live Inspector
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
        {!isDesktop() && adminAuthLocked && (
          <div className="card row" style={{ borderColor: "var(--warn)", flexWrap: "wrap" }}>
            <div>
              <strong>Admin Authentication Required</strong>
              <div style={{ color: "var(--muted)", fontSize: 12 }}>
                Enter the gateway <code>admin_password</code> (stored in ephemeral browser{" "}
                <code>sessionStorage</code> only, sent via <code>x-admin-password</code>).
              </div>
            </div>
            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              <input
                type="password"
                value={webAdminPrompt}
                onChange={(e) => setWebAdminPrompt(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleUnlockWebAdmin();
                }}
                placeholder="Admin password"
              />
              <button className="btn" onClick={handleUnlockWebAdmin}>
                Unlock Console
              </button>
            </div>
          </div>
        )}

        {!isDesktop() && !adminAuthLocked && getSessionAdminPassword() && (
          <div className="card row" style={{ padding: "8px 14px" }}>
            <span style={{ fontSize: 12, color: "var(--muted)" }}>
              Authenticated via browser <code>sessionStorage</code> (<code>x-admin-password</code>).
            </span>
            <button className="btn secondary" onClick={handleLockWebAdmin}>
              Lock Console
            </button>
          </div>
        )}

        {notice && (
          <div className="card row">
            <span>{notice}</span>
            <button className="btn secondary" onClick={() => setNotice("")}>
              Dismiss
            </button>
          </div>
        )}

        {tab === "channels" && (
          <ChannelsPage
            routeMode={routeMode}
            channels={channels}
            channelConfigs={channelConfigs}
            autostart={autostart}
            activeAuth={activeAuth}
            cloudStatus={cloudStatus}
            onRouteModeChange={setRouteMode}
            onAutostartChange={setAutostart}
            onActiveAuthChange={setActiveAuth}
            onCloudStatusChange={setCloudStatus}
            onRefresh={refreshAll}
            onNotice={setNotice}
            onError={handleApiError}
          />
        )}

        {tab === "inspector" && (
          <InspectorPage
            bindAddr={bindAddr}
            entries={trafficEntries}
            onRefresh={refreshAll}
            onNotice={setNotice}
            onError={handleApiError}
          />
        )}

        {tab === "takeover" && (
          <ClientsPage
            profiles={profiles}
            onRefresh={refreshAll}
            onNotice={setNotice}
            onError={handleApiError}
          />
        )}

        {tab === "trajectory" && (
          <TrajectoryPage
            bindAddr={bindAddr}
            sessions={sessions}
            selectedSession={selectedSession}
            steps={steps}
            onSelectSession={setSelectedSession}
            onStepsChange={setSteps}
            onNotice={setNotice}
            onError={handleApiError}
          />
        )}

        {tab === "savings" && (
          <SavingsPage
            bindAddr={bindAddr}
            savings={savings}
            savingsDays={savingsDays}
            onSavingsDaysChange={setSavingsDays}
          />
        )}
      </main>
    </div>
  );
}
