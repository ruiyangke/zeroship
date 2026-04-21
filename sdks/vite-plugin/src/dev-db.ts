// sdks/vite-plugin/src/dev-db.ts
//
// Zero-setup local-dev Postgres for creators who don't want to install
// Postgres or run Docker. Spawns PGlite (WASM Postgres) behind a TCP
// socket on a random port, persists data to `.zeroship/dev.db/`, and
// hands a DATABASE_URL back to the caller for forwarding to the
// spawned runtime.
//
// Used by dev-server.ts when DATABASE_URL is not provided via .env or
// the parent environment. Transparent: same wire protocol as real
// Postgres, so `crates/plugin-db` (compio-postgres client) talks to it
// unchanged.

import { PGlite } from "@electric-sql/pglite";
import { PGLiteSocketServer } from "@electric-sql/pglite-socket";
import { mkdirSync } from "node:fs";
import { createServer } from "node:net";
import { resolve } from "node:path";

export interface DevPostgres {
  /** postgres://user:pass@127.0.0.1:<port>/postgres */
  databaseUrl: string;
  /** Stop the socket server and close the PGlite instance. */
  stop: () => Promise<void>;
}

// Pre-allocate a free port before starting PGLiteSocketServer. Avoids
// reaching into pglite-socket's private `.server` field to read the
// bound port after the fact. Small race window (another process could
// grab the port between close and PGLiteSocketServer.listen) — in dev
// that's a non-issue.
async function findOpenPort(): Promise<number> {
  return new Promise((resolveFn, reject) => {
    const srv = createServer();
    srv.unref(); // don't keep the process alive
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (addr && typeof addr === "object" && typeof addr.port === "number") {
        const { port } = addr;
        srv.close(() => resolveFn(port));
      } else {
        srv.close(() => reject(new Error("pre-allocate: address() returned no port")));
      }
    });
    srv.on("error", reject);
  });
}

/**
 * Start a PGlite-backed Postgres on localhost with an OS-assigned port.
 * Data persists across restarts in `<projectRoot>/.zeroship/dev.db/`.
 */
export async function startDevPostgres(projectRoot: string): Promise<DevPostgres> {
  const dataDir = resolve(projectRoot, ".zeroship/dev.db");
  mkdirSync(dataDir, { recursive: true });

  const port = await findOpenPort();
  const db = await PGlite.create({ dataDir });
  const server = new PGLiteSocketServer({
    db,
    port,
    host: "127.0.0.1",
    // Our compio-postgres pool warms 2 connections on connect() by default,
    // and a single worker can burst to ~8. Pglite-socket defaults to
    // maxConnections=1 — which would reject the pool's second warm-up
    // connection. Multiplex up to 16 simultaneous sockets (pglite-socket
    // serializes queries onto the single underlying PGlite instance via
    // its internal queue, so this is purely a connection-accept limit).
    maxConnections: 16,
    debug: process.env.ZEROSHIP_DEV_DB_DEBUG === "1",
  });
  await server.start();

  // `sslmode=disable` is required: PGlite-socket is a plain TCP proxy into
  // PGlite's in-process query engine — it doesn't speak SSL. Without this
  // our compio-postgres client defaults to `Prefer`, sends an SSLRequest
  // packet, and PGlite handles that as an unknown startup message → hang
  // / "error connecting to server". Disabling SSL keeps the handshake on
  // the raw wire where pglite-socket can parse it.
  const databaseUrl = `postgres://postgres:postgres@127.0.0.1:${port}/postgres?sslmode=disable`;

  const stop = async () => {
    try { await server.stop(); } catch { /* noop — server already closed */ }
    try { await db.close(); } catch { /* noop */ }
  };

  return { databaseUrl, stop };
}
