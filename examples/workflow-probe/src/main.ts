// Browser checks load this entry and call the workflow RPC surface.
const out = document.getElementById("out");
if (out) {
  out.textContent =
    "workflow-probe has no UI. POST /__zeroship/v1/wf.start {\"case\":\"basic\"}.";
}
