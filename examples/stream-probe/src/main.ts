// This app is driven over the wire, not opened in a browser -- but the import
// below is NOT decorative. Server procedures are discovered through the client
// module graph, so a `src/index.ts` that nothing imports contributes zero
// procedures and the build reports "0 server functions" while still emitting a
// manifest. Keep the import.
import * as api from "./index";

const out = document.getElementById("out");
if (out) out.textContent = `stream-probe: ${Object.keys(api).join(", ")}`;
