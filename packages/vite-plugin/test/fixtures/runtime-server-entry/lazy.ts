import { env, currentRequestId } from "zeroship";
import { query, stream } from "@zeroship/rpc/server";
import { state, input } from "./state";

state.loaded++;
state.loaderKind = env.probe.kind();

export const lazy = query(async (value, ctx) => {
  state.invoked++;
  await new Promise(resolve => setTimeout(resolve, 1));
  return { value, kind: env.probe.kind(), sameRequest: currentRequestId() === ctx.requestId };
}, { id: "__proto__", lazy: true, input });
Object.freeze(lazy.config);
Object.freeze(lazy);

export const tokens = stream(async function* (value, ctx) {
  yield value;
  await Promise.resolve();
  yield currentRequestId() === ctx.requestId ? "same request" : "wrong request";
}, { id: "tokens", lazy: true, outputIsString: true });
