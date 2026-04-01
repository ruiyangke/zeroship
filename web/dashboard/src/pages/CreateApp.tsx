import { useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { createApp } from "../api";

const PLANS = ["free", "starter", "pro", "enterprise"];

export default function CreateApp() {
  const navigate = useNavigate();
  const [appId, setAppId] = useState("");
  const [planId, setPlanId] = useState("free");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!appId.trim()) return;

    setLoading(true);
    setError("");

    try {
      const app = await createApp(appId.trim(), planId);
      navigate(`/apps/${app.id}`);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "failed to create app");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div>
      <div className="page-header">
        <h1>// create app</h1>
      </div>

      <div className="card" style={{ maxWidth: 480 }}>
        {error && <div className="error-message">{error}</div>}

        <form onSubmit={handleSubmit}>
          <div className="form-group">
            <label htmlFor="app-id">app id</label>
            <input
              id="app-id"
              type="text"
              className="form-input"
              placeholder="my-app"
              value={appId}
              onChange={(e) => setAppId(e.target.value)}
              autoFocus
              pattern="[a-zA-Z0-9_-]+"
              title="alphanumeric, dashes, and underscores only"
            />
          </div>

          <div className="form-group">
            <label htmlFor="plan-id">plan</label>
            <select
              id="plan-id"
              className="form-input"
              value={planId}
              onChange={(e) => setPlanId(e.target.value)}
            >
              {PLANS.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          </div>

          <button type="submit" className="btn btn-primary" disabled={loading || !appId.trim()}>
            {loading ? "creating..." : "create"}
          </button>
        </form>
      </div>
    </div>
  );
}
