export interface SystemRow {
  id: string;
  version: number;
  created_at: number;
  updated_at: number;
  created_by: string | null;
  updated_by: string | null;
  deleted_at: number | null;
}
interface Workspace extends SystemRow { slug: string; tier: string }
interface User extends SystemRow { handle: string; contactEmail: string; ssn: string }
export interface Task extends SystemRow { title: string }

interface Outputs {
  "db-e2e.health": { ok: boolean };
  "db-e2e.seed-demo": { workspaces: Record<"alpha" | "beta", Workspace>; users: Record<"alice" | "bob", User>; tasks: Record<"launch" | "triage", Task> };
  "db-e2e.upsert-workspace": Workspace;
  "db-e2e.create-task": Task;
  "db-e2e.get-task": Task | null;
  "db-e2e.query-showcase": {
    byId: Task; byFilter: Task; firstOpen: Task; uniqueUser: User; filtered: Task[]; afterPage: Task[];
    countActive: number; distinctStatuses: string[]; aggregate: { status: string; count: number }[];
  };
  "db-e2e.tasks-with-relations": (Task & { owner: User; workspace: Workspace })[];
  "db-e2e.update-task-versioned": { ok: boolean; task: Task; code?: string };
  "db-e2e.soft-delete-task": { deleted: Task; visibleAfterDelete: Task | null; countAfterDelete: number };
  "db-e2e.restore-task": { restored: Task; countAfterRestore: number };
  "db-e2e.purge-task": { visibleAfterPurge: Task | null; countAfterPurge: number };
  "db-e2e.transaction-showcase": {
    commit: { created: Task }; rollback: { errorCode: string; visibleCount: number };
    nested: { innerErrorCode: string; outerCount: number; innerCount: number };
  };
  "db-e2e.search-showcase": { vector: { name: string }[]; near: { name: string }[] };
  "db-e2e.security-showcase": {
    can: { support: boolean; guest: boolean }; deniedCode: string; plainSsn: string;
    rowReveal: { contactEmail: string }; bulk: Record<string, { ssn: string }>; hinted: { ssn: string; contactEmail: string };
  };
}

export async function rpc<K extends keyof Outputs>(baseUrl: string, name: K, input: Record<string, unknown> = {}): Promise<Outputs[K]> {
  const response = await fetch(`${baseUrl}/__zeroship/v1/${name}`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: input }),
    signal: AbortSignal.timeout(10_000),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`${name}: HTTP ${response.status}: ${text}`);
  const parsed: unknown = JSON.parse(text);
  if (!parsed || typeof parsed !== "object" || !("json" in parsed)) throw new Error(`${name}: missing JSON envelope: ${text}`);
  return parsed.json as Outputs[K];
}

export async function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} timed out`)), ms);
    })]);
  } finally { clearTimeout(timer); }
}

class ValueStream {
  private decoder = new TextDecoder();
  private buffer = "";
  constructor(private reader: ReadableStreamDefaultReader<Uint8Array>, private controller: AbortController) {}
  async nextValue(): Promise<Task[]> {
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline !== -1) {
        const line = this.buffer.slice(0, newline).trim();
        this.buffer = this.buffer.slice(newline + 1);
        if (line.startsWith("2:")) {
          const frame: unknown = JSON.parse(line.slice(2));
          if (!Array.isArray(frame) || !Array.isArray(frame[0])) throw new Error(`Malformed data frame: ${line}`);
          return frame[0] as Task[];
        }
        if (line.startsWith("d:")) throw new Error("Stream terminated before next value");
      } else {
        const { value, done } = await this.reader.read();
        if (done) throw new Error("Stream ended before next value");
        this.buffer += this.decoder.decode(value, { stream: true });
      }
    }
  }
  async close() { await this.reader.cancel(); this.controller.abort(); }
}

export async function openValueStream(baseUrl: string, name: string, input: Record<string, unknown>) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 10_000);
  try {
    const encoded = Buffer.from(JSON.stringify({ json: input })).toString("base64url");
    const response = await fetch(`${baseUrl}/__zeroship/v1/${name}?input=${encoded}`, {
      headers: { Accept: "text/event-stream" }, signal: controller.signal,
    });
    if (!response.ok || !response.body) throw new Error(`Stream open: HTTP ${response.status}: ${await response.text()}`);
    return new ValueStream(response.body.getReader(), controller);
  } catch (error) { controller.abort(); throw error; }
  finally { clearTimeout(timer); }
}

export async function waitForFrame(stream: ValueStream, label: string, predicate: (rows: Task[]) => boolean) {
  return withTimeout((async () => {
    for (;;) { const rows = await stream.nextValue(); if (predicate(rows)) return rows; }
  })(), 5_000, label);
}
