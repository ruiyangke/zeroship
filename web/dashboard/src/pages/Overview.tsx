import { useState, useEffect, useRef } from "react";
import { getStats, getHealth, getAllUsage, type Stats, type HealthStatus, type AppUsage } from "../api";
import StatusBadge from "../components/StatusBadge";

export default function Overview() {
  const [stats, setStats] = useState<Stats | null>(null);
  const [health, setHealth] = useState<HealthStatus | null>(null);
  const [usage, setUsage] = useState<AppUsage[]>([]);
  const [error, setError] = useState("");
  const intervalRef = useRef<number | null>(null);

  function fetchAll() {
    Promise.all([getStats(), getHealth(), getAllUsage()])
      .then(([s, h, u]) => {
        setStats(s);
        setHealth(h);
        setUsage(u);
        setError("");
      })
      .catch((err) => setError(err.message));
  }

  useEffect(() => {
    fetchAll();
    intervalRef.current = window.setInterval(fetchAll, 5000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, []);

  const totalRequests = usage.reduce((sum, u) => {
    return sum + Object.values(u.counters || {}).reduce((a, b) => a + b, 0);
  }, 0);

  const utilization = stats
    ? stats.max_isolates > 0
      ? Math.round((stats.active_isolates / stats.max_isolates) * 100)
      : 0
    : 0;

  return (
    <div>
      <div className="page-header">
        <h1>// overview</h1>
        <span className="text-secondary" style={{ fontSize: 11 }}>
          auto-refresh: 5s
        </span>
      </div>

      {error && <div className="error-message">{error}</div>}

      <div className="stats-grid">
        <div className="stat-card">
          <div className="stat-label">health</div>
          <div className="stat-value accent">
            {health ? health.status : "--"}
          </div>
        </div>
        <div className="stat-card">
          <div className="stat-label">total apps</div>
          <div className="stat-value">
            {stats ? stats.apps.length : "--"}
          </div>
        </div>
        <div className="stat-card">
          <div className="stat-label">active isolates</div>
          <div className="stat-value">
            {stats ? `${stats.active_isolates} / ${stats.max_isolates}` : "--"}
          </div>
        </div>
        <div className="stat-card">
          <div className="stat-label">pool utilization</div>
          <div className="stat-value">{stats ? `${utilization}%` : "--"}</div>
        </div>
        <div className="stat-card">
          <div className="stat-label">total requests</div>
          <div className="stat-value">{stats ? totalRequests : "--"}</div>
        </div>
      </div>

      {stats && stats.apps.length > 0 && (
        <div className="card">
          <div className="card-header">
            <h2>isolate pool</h2>
          </div>
          <table className="data-table">
            <thead>
              <tr>
                <th>app id</th>
                <th>requests</th>
                <th>idle</th>
                <th>status</th>
              </tr>
            </thead>
            <tbody>
              {stats.apps.map((app) => {
                const status: "running" | "idle" | "stopped" =
                  app.request_count > 0 && app.idle_secs < 5
                    ? "running"
                    : app.idle_secs < 60
                      ? "idle"
                      : "stopped";
                return (
                  <tr key={app.app_id}>
                    <td>{app.app_id}</td>
                    <td>{app.request_count}</td>
                    <td>{app.idle_secs}s</td>
                    <td>
                      <StatusBadge status={status} />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
