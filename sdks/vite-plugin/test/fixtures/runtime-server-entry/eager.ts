import { env, currentRequestId } from "zeroship";
import { query } from "@zeroship/rpc/server";
import { input } from "./state";

export const eager = query(async (value, ctx) => {
  await Promise.resolve();
  return { value, kind: env.probe.kind(), sameRequest: currentRequestId() === ctx.requestId };
}, { id: "eager", input });
