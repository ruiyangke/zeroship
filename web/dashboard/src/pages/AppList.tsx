import { useState, useEffect } from "react";
import { Link, useNavigate } from "react-router-dom";
import { listApps, getStats, type AppRecord, type Stats } from "../api";
import StatusBadge from "../components/StatusBadge";

export default function AppList() {
  const [apps, setApps] = useState<AppRecord[]>([]);
  const [stats, setStats] = useState<Stats | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const navigate = useNavigate();

  useEffect(() => {
    Promise.all([listApps(), getStats()])
      .then(([a, s]) => {
        setApps(a);
        setStats(s);
      })
      .catch((err) => setError(err.message))
      .finally(() => setLoading(false));
  }, []);

  function getAppStatus(appId: string): "running" | "idle" | "stopped" {
    if (!stats) return "stopped";
    const entry = stats.apps.find((a) => a.app_id === appId);
    if (!entry) return "stopped";
    if (entry.request_count > 0 && entry.idle_secs < 5) return "running";
    if (entry.idle_secs < 60) return "idle";
    return "stopped";
  }

  function getAppRequests(appId: string): number {
    if (!stats) return 0;
    const entry = stats.apps.find((a) => a.app_id === appId);
    return entry?.request_count ?? 0;
  }

  return (
    <div>
      <div className="page-header">
        <h1>// apps</h1>
        <Link to="/apps/new" className="btn btn-primary">
          + new app
        </Link>
      </div>

      {error && <div className="error-message">{error}</div>}

      {loading ? (
        <div className="loading">loading apps...</div>
      ) : apps.length === 0 ? (
        <div className="empty-state">
          no apps found — <Link to="/apps/new">create one</Link>
        </div>
      ) : (
        <div className="card" style={{ padding: 0 }}>
          <table className="data-table">
            <thead>
              <tr>
                <th>id</th>
                <th>plan</th>
                <th>version</th>
                <th>requests</th>
                <th>status</th>
                <th>updated</th>
              </tr>
            </thead>
            <tbody>
              {apps.map((app) => (
                <tr
                  key={app.id}
                  style={{ cursor: "pointer" }}
                  onClick={() => navigate(`/apps/${app.id}`)}
                >
                  <td>
                    <Link to={`/apps/${app.id}`} onClick={(e) => e.stopPropagation()}>
                      {app.id}
                    </Link>
                  </td>
                  <td>{app.plan_id}</td>
                  <td>v{app.version}</td>
                  <td>{getAppRequests(app.id)}</td>
                  <td>
                    <StatusBadge status={getAppStatus(app.id)} />
                  </td>
                  <td className="text-secondary">
                    {new Date(app.updated_at).toLocaleDateString()}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
