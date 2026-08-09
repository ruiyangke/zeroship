"use server";

// A stream procedure with NO external dependency, so the thing being measured
// is the platform's SSE path rather than some provider's latency.
//
// Why this app exists: `examples/ai-chat` is the only other app with a
// `stream` procedure, and it calls a paid API. Testing chunk delivery through
// it would mostly test that provider, and could not run without spend. This
// emits its own chunks on a local timer instead.
//
// The property under test is that chunk BOUNDARIES survive the gateway. A
// proxy that buffers a response body would still deliver every chunk and every
// byte, in order, and a test that only compared the final assembled text would
// pass while streaming was completely broken. So the emission is paced: with
// CHUNKS chunks GAP_MS apart, a streaming path delivers the first chunk after
// roughly one GAP_MS and the last after roughly CHUNKS x GAP_MS, while a
// buffering path delivers all of them at the end. Time-to-first-chunk is the
// discriminator, and the margin between the two outcomes is ~5x rather than a
// few milliseconds, so the check does not rest on tight timing.

import { stream, query } from "@zeroship/rpc/server";

const CHUNKS = 5;
const GAP_MS = 200;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export const ticks = stream(
  async function* () {
    for (let i = 1; i <= CHUNKS; i++) {
      await sleep(GAP_MS);
      // `mark` is a fixed token the harness greps for; `i` orders the chunks;
      // `atMs` is the server's own clock, which lets a reader tell a slow
      // server apart from a buffering proxy without trusting client timing.
      yield { mark: "TICK", i, of: CHUNKS, atMs: Date.now() };
    }
  },
  { id: "probe.ticks" },
);

// A non-streaming companion so the harness can prove the app is reachable and
// the transport healthy BEFORE it starts reasoning about chunk timing. Without
// this, a connection failure and a buffered stream look alike: no chunks.
export const ping = query(async () => ({ ok: true, chunks: CHUNKS, gapMs: GAP_MS }), {
  id: "probe.ping",
});
