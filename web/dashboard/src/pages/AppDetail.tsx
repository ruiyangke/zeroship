import { useState, useEffect } from "react";
import { useParams, useNavigate, Link } from "react-router-dom";
import { getApp, getAppUsage, deployApp, deleteApp, updatePlan, type AppRecord, type UsageCounters } from "../api";

const PLANS = ["free", "starter", "pro", "enterprise"];

export default function AppDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();

  const [app, setApp] = useState<AppRecord | null>(null);
  const [usage, setUsage] = useState<UsageCounters | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  // Deploy state
  const [code, setCode] = useState("");
  const [deploying, setDeploying] = useState(false);
  const [deployResult, setDeployResult] = useState<{ ok: boolean; msg: string } | null>(null);

  // Plan change state
  const [newPlan, setNewPlan] = useState("");
  const [changingPlan, setChangingPlan] = useState(false);

  // Copy state
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!id) return;
    setLoading(true);
    Promise.all([getApp(id), getAppUsage(id).catch(() => null)])
      .then(([a, u]) => {
        setApp(a);
        setUsage(u);
        setNewPlan(a.plan_id);
      })
      .catch((err) => setError(err.message))
      .finally(() => setLoading(false));
  }, [id]);

  async function handleDeploy() {
    if (!id || !code.trim()) return;
    setDeploying(true);
    setDeployResult(null);
    try {
      const res = await deployApp(id, code);
      setDeployResult({ ok: true, msg: `deployed version ${res.version}` });
      // Refresh app data
      const updated = await getApp(id);
      setApp(updated);
    } catch (err: unknown) {
      setDeployResult({ ok: false, msg: err instanceof Error ? err.message : "deploy failed" });
    } finally {
      setDeploying(false);
    }
  }

  async function handleDelete() {
    if (!id) return;
    const confirmed = window.confirm(`delete app "${id}"? this cannot be undone.`);
    if (!confirmed) return;
    try {
      await deleteApp(id);
      navigate("/apps");
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "delete failed");
    }
  }

  async function handlePlanChange() {
    if (!id || !newPlan || newPlan === app?.plan_id) return;
    setChangingPlan(true);
    try {
      await updatePlan(id, newPlan);
      const updated = await getApp(id);
      setApp(updated);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "plan change failed");
    } finally {
      setChangingPlan(false);
    }
  }

  function handleCopy() {
    if (!app) return;
    navigator.clipboard.writeText(app.api_key).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }

  if (loading) return <div className="loading">loading...</div>;
  if (error && !app) return <div className="error-message">{error}</div>;
  if (!app) return <div className="error-message">app not found</div>;

  const totalRequests = usage
    ? Object.values(usage.counters || {}).reduce((a, b) => a + b, 0)
    : 0;

  return (
    <div>
      <div className="breadcrumb">
        <Link to="/apps">apps</Link>
        <span className="separator">/</span>
        <span>{app.id}</span>
      </div>

      <div className="page-header">
        <h1>// {app.id}</h1>
      </div>

      {error && <div className="error-message">{error}</div>}

      {/* Info Panel */}
      <div className="card">
        <div className="card-header">
          <h2>info</h2>
        </div>
        <div className="info-grid">
          <div className="info-item">
            <div className="info-label">app id</div>
            <div className="info-value">{app.id}</div>
          </div>
          <div className="info-item">
            <div className="info-label">plan</div>
            <div className="info-value">{app.plan_id}</div>
          </div>
          <div className="info-item">
            <div className="info-label">version</div>
            <div className="info-value">v{app.version}</div>
          </div>
          <div className="info-item">
            <div className="info-label">api key</div>
            <div className="info-value">
              <div className="copy-wrap">
                <span style={{ flex: 1 }}>{app.api_key}</span>
                <button className={`copy-btn ${copied ? "copied" : ""}`} onClick={handleCopy}>
                  {copied ? "copied" : "copy"}
                </button>
              </div>
            </div>
          </div>
          <div className="info-item">
            <div className="info-label">created</div>
            <div className="info-value">{new Date(app.created_at).toLocaleString()}</div>
          </div>
          <div className="info-item">
            <div className="info-label">updated</div>
            <div className="info-value">{new Date(app.updated_at).toLocaleString()}</div>
          </div>
        </div>
      </div>

      {/* Usage Panel */}
      <div className="card">
        <div className="card-header">
          <h2>usage</h2>
        </div>
        {usage ? (
          <div className="stats-grid">
            <div className="stat-card">
              <div className="stat-label">total requests</div>
              <div className="stat-value">{totalRequests}</div>
            </div>
            {Object.entries(usage.counters || {}).map(([key, val]) => (
              <div className="stat-card" key={key}>
                <div className="stat-label">{key}</div>
                <div className="stat-value">{val}</div>
              </div>
            ))}
          </div>
        ) : (
          <div className="text-secondary" style={{ fontSize: 13 }}>
            no usage data available
          </div>
        )}
      </div>

      {/* Deploy Panel */}
      <div className="card">
        <div className="card-header">
          <h2>deploy</h2>
        </div>
        <div className="form-group">
          <label htmlFor="deploy-code">javascript source</label>
          <textarea
            id="deploy-code"
            className="form-input"
            placeholder="// paste your handler code here..."
            value={code}
            onChange={(e) => setCode(e.target.value)}
          />
        </div>
        <button
          className="btn btn-primary"
          onClick={handleDeploy}
          disabled={deploying || !code.trim()}
        >
          {deploying ? "deploying..." : "deploy"}
        </button>
        {deployResult && (
          <div className={`deploy-result ${deployResult.ok ? "success" : "error"}`}>
            {deployResult.msg}
          </div>
        )}
      </div>

      {/* Danger Zone */}
      <div className="card danger-zone">
        <div className="card-header">
          <h2>danger zone</h2>
        </div>
        <div className="danger-actions">
          <div className="form-group">
            <label htmlFor="plan-select">change plan</label>
            <div style={{ display: "flex", gap: 8 }}>
              <select
                id="plan-select"
                className="form-input"
                value={newPlan}
                onChange={(e) => setNewPlan(e.target.value)}
                style={{ flex: 1 }}
              >
                {PLANS.map((p) => (
                  <option key={p} value={p}>
                    {p}
                  </option>
                ))}
              </select>
              <button
                className="btn btn-danger btn-small"
                onClick={handlePlanChange}
                disabled={changingPlan || newPlan === app.plan_id}
              >
                {changingPlan ? "..." : "update"}
              </button>
            </div>
          </div>
        </div>
        <div className="mt-16">
          <button className="btn btn-danger" onClick={handleDelete}>
            delete app
          </button>
        </div>
      </div>
    </div>
  );
}
