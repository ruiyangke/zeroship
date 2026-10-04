import { expect, test } from "vitest";
import { StreamPacer } from "../src/pacer";

function harness() {
  let now = 0;
  const pauses: number[] = [];
  return {
    now: () => now,
    advance: (ms: number) => {
      now += ms;
    },
    sleep: async (ms: number) => {
      pauses.push(ms);
      now += ms;
    },
    pauses,
  };
}

test("a wait between work sections adds nothing to the pause", async () => {
  const env = harness();
  const pacer = new StreamPacer({ now: env.now, sleep: env.sleep, thresholdMs: 10 });
  await pacer.work(() => env.advance(6));
  env.advance(1000);
  await pacer.work(() => env.advance(6));
  expect(env.pauses).toEqual([12]);
});

test("the pause equals the measured work once the threshold is passed", async () => {
  const env = harness();
  const pacer = new StreamPacer({ now: env.now, sleep: env.sleep, thresholdMs: 10 });
  await pacer.work(() => env.advance(25));
  expect(env.pauses).toEqual([25]);
});

test("no pause below the threshold", async () => {
  const env = harness();
  const pacer = new StreamPacer({ now: env.now, sleep: env.sleep, thresholdMs: 10 });
  await pacer.work(() => env.advance(4));
  await pacer.work(() => env.advance(4));
  expect(env.pauses).toEqual([]);
});
