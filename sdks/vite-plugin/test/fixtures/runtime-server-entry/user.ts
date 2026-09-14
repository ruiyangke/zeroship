import { env } from "zeroship";
import { state } from "./state";

export default {
  label: "original receiver",
  fetch(request: Request) {
    return Response.json({ label: this.label, path: new URL(request.url).pathname, kind: env.probe.kind() });
  },
  rpc: { inspect: () => ({ ...state }) },
};
