export function About({ navigate }: { navigate: (to: string) => void }) {
  return (
    <main style={{ fontFamily: "system-ui, sans-serif", maxWidth: 480, margin: "40px auto", padding: "0 16px" }}>
      <h1>About</h1>
      <p>
        This is the CSR demo. The server has exactly one RPC method —
        <code> listTodos</code> — exposed at
        <code> POST /_rpc/src/server/listTodos</code>. Every other URL
        is served the SPA shell (<code>/index.html</code>) and routed
        client-side.
      </p>
      <p>
        <a href="/" onClick={(e) => { e.preventDefault(); navigate("/"); }}>
          ← Back home
        </a>
      </p>
    </main>
  );
}
