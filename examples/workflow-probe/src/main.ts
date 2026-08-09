// The browser half is deliberately inert. Everything this app exists to prove
// is server-side and durable, so the harness drives the RPC endpoints directly
// and never opens a page. index.html only exists so the vite client build has
// an entry.
const out = document.getElementById("out");
if (out) {
  out.textContent =
    "workflow-probe has no UI. POST /__zeroship/v1/wf.start {\"case\":\"basic\"}.";
}
