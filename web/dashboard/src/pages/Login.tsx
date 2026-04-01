import { useState, type FormEvent } from "react";
import { getHealth } from "../api";

interface LoginProps {
  onLogin: () => void;
}

export default function Login({ onLogin }: LoginProps) {
  const [key, setKey] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!key.trim()) return;

    setLoading(true);
    setError("");

    // Store key temporarily to test it
    localStorage.setItem("appbase_key", key.trim());

    try {
      await getHealth();
      onLogin();
    } catch {
      localStorage.removeItem("appbase_key");
      setError("connection failed — check your key and that the server is running on :3333");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="login-page">
      <div className="login-box">
        <h1>appbase</h1>
        <div className="subtitle">enter master key to continue</div>
        <form onSubmit={handleSubmit}>
          <div className="form-group">
            <label htmlFor="master-key">master key</label>
            <input
              id="master-key"
              type="password"
              className="form-input"
              placeholder="sk_..."
              value={key}
              onChange={(e) => setKey(e.target.value)}
              autoFocus
            />
          </div>
          <button type="submit" className="btn btn-primary" disabled={loading || !key.trim()}>
            {loading ? "connecting..." : "authenticate"}
          </button>
          {error && <div className="login-error">{error}</div>}
        </form>
      </div>
    </div>
  );
}
