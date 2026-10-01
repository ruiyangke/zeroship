// An ES module dependency shaped like langsmith's Node build
// (`dist/utils/worker_threads.js` with `dist/utils/serialize_worker.js`): it
// believes worker threads are available, constructs a `Worker` lazily inside
// try/catch, and serializes inline when the constructor throws. A `Worker`
// that constructs but never runs leaves its caller awaiting a reply forever,
// so `probe()` reports `no reply` after a bound instead of waiting.

export const OFF_THREAD_SERIALIZER_PACKAGE = JSON.stringify({
  name: "serializes-off-thread",
  type: "module",
  exports: "./index.js",
});

export const OFF_THREAD_SERIALIZER_SOURCE = `import { Worker } from "node:worker_threads";

const WORKER_THREADS_AVAILABLE = true;
const WORKER_SOURCE = \`
const { parentPort } = require("worker_threads");
parentPort.on("message", ({ id, payload }) => parentPort.postMessage({ id, text: JSON.stringify(payload) }));
\`;
const pending = new Map();
let worker = null;
let disabled = false;
let nextId = 0;

function start() {
  if (!WORKER_THREADS_AVAILABLE || Worker === null) {
    disabled = true;
    return false;
  }
  try {
    const started = new Worker(WORKER_SOURCE, { eval: true });
    started.on("message", (message) => {
      pending.get(message.id)?.(message.text);
      pending.delete(message.id);
    });
    started.unref();
    worker = started;
    return true;
  } catch {
    disabled = true;
    return false;
  }
}

async function serializeOffThread(payload) {
  if (disabled || (worker === null && !start())) return null;
  const id = nextId++;
  return new Promise((resolve) => {
    pending.set(id, resolve);
    worker.postMessage({ id, op: "serialize", payload });
  });
}

// How a payload large enough to go off-thread was serialized.
export function probe() {
  const serialized = serializeOffThread({ text: "x".repeat(64 * 1024) })
    .then((text) => (text === null ? "inline" : "off-thread"));
  const bound = new Promise((resolve) => setTimeout(() => resolve("no reply"), 250));
  return Promise.race([serialized, bound]);
}
`;
