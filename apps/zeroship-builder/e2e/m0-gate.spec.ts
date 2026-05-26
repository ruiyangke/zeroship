import { readFile, mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

import { test, expect, type Page } from "@playwright/test";

interface PromptCase {
  id: string;
  title: string;
  prompt: string;
  criteria: string;
}

interface PromptOutcome {
  id: string;
  title: string;
  status: "pass" | "fail" | "skipped";
  appId?: string;
  appName?: string;
  deployHash?: string;
  url?: string;
  reachableStatus?: number;
  bodyExcerpt?: string;
  durationMs?: number;
  error?: string;
  ranAt: string;
}

const ROOT = resolve(new URL("../../..", import.meta.url).pathname);
const PROMPTS_PATH = resolve(new URL("./m0-prompts.json", import.meta.url).pathname);
const RESULTS_PATH = process.env.M0_RESULTS_PATH
  ?? resolve(ROOT, ".zeroship/m0-gate/results.json");
const LOG_DIR = resolve(ROOT, ".zeroship/m0-gate/logs");

const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";
const CONTROL_KEY = process.env.CONTROL_KEY ?? "dev-master-key";
const GATEWAY_URL = process.env.GATEWAY_URL ?? "http://localhost:8000";
const BUILDER_URL = process.env.BUILDER_URL ?? "http://localhost:3001";
const SANDBOX_URL = process.env.SANDBOX_URL ?? "http://localhost:9091";
const PER_PROMPT_TIMEOUT_MS = Number(process.env.M0_PROMPT_TIMEOUT_MS ?? 330_000);
const REACHABLE_TIMEOUT_MS = Number(process.env.M0_REACHABLE_TIMEOUT_MS ?? 120_000);
const MAX_SURVEY_RESUMES = Number(process.env.M0_MAX_SURVEY_RESUMES ?? 2);

async function loadPrompts(): Promise<PromptCase[]> {
  return JSON.parse(await readFile(PROMPTS_PATH, "utf8")) as PromptCase[];
}

async function loadPreviousOutcomes(): Promise<Record<string, PromptOutcome>> {
  try {
    return JSON.parse(await readFile(RESULTS_PATH, "utf8")) as Record<string, PromptOutcome>;
  } catch {
    return {};
  }
}

async function saveOutcome(outcome: PromptOutcome): Promise<void> {
  const current = await loadPreviousOutcomes();
  current[outcome.id] = outcome;
  await mkdir(dirname(RESULTS_PATH), { recursive: true });
  await writeFile(RESULTS_PATH, JSON.stringify(current, null, 2));
}

function selectedPrompts(all: PromptCase[]): PromptCase[] {
  const only = process.env.M0_ONLY?.split(",").map((s) => s.trim()).filter(Boolean);
  let out = only?.length ? all.filter((p) => only.includes(p.id)) : all;
  const limit = Number(process.env.M0_LIMIT ?? 0);
  if (limit > 0) out = out.slice(0, limit);
  return out;
}

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(2500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

async function controlFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${CONTROL_KEY}`);
  if (init.body && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  return fetch(`${CONTROL_URL}${path}`, { ...init, headers });
}

function slugPart(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 24);
}

async function createControlApp(prompt: PromptCase): Promise<{
  id: string;
  name: string;
  api_key?: string;
}> {
  const suffix = Date.now().toString(36).slice(-6);
  const name = `m0-${slugPart(prompt.id)}-${suffix}`;
  const res = await controlFetch("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id: "free" }),
  });
  if (!res.ok) {
    throw new Error(`create app failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as { id: string; name: string; api_key?: string };
}

async function getDeployHash(appId: string): Promise<string | null> {
  const res = await controlFetch(`/api/apps/${encodeURIComponent(appId)}`);
  if (!res.ok) {
    throw new Error(`get app failed (${res.status}): ${await res.text()}`);
  }
  const json = await res.json() as { deploy_hash?: string | null };
  return json.deploy_hash ?? null;
}

function userMessage(text: string) {
  return {
    id: `msg-${crypto.randomUUID()}`,
    role: "user",
    parts: [{ type: "text", text }],
  };
}

function harnessPrompt(prompt: PromptCase): string {
  return [
    prompt.prompt,
    "",
    "Please implement this in the existing React/Vite project, keep it small, use browser-local state only, run a build check, and deploy it when ready.",
  ].join("\n");
}

interface SseReadResult {
  chunks: any[];
  rawFrames: string[];
  parseErrors: string[];
}

async function readSseChunks(res: Response): Promise<SseReadResult> {
  if (!res.body) throw new Error("chat response had no body");
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const chunks: any[] = [];
  const rawFrames: string[] = [];
  const parseErrors: string[] = [];

  const consumeFrame = (frame: string) => {
    for (const line of frame.split("\n")) {
      if (!line.startsWith("data: ")) continue;
      const data = line.slice("data: ".length).trim();
      if (!data) continue;
      rawFrames.push(data);
      if (data === "[DONE]") continue;
      try {
        chunks.push(JSON.parse(data));
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        parseErrors.push(`invalid SSE JSON: ${message}; data=${data.slice(0, 500)}`);
      }
    }
  };

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let boundary = buffer.indexOf("\n\n");
    while (boundary >= 0) {
      const frame = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");
      consumeFrame(frame);
    }
  }
  const tail = buffer.trim();
  if (tail) consumeFrame(tail);
  return { chunks, rawFrames, parseErrors };
}

function parseToolOutput(output: unknown): any {
  if (typeof output !== "string") return output;
  try {
    return JSON.parse(output);
  } catch {
    return output;
  }
}

function safeLogName(value: string): string {
  return value.replace(/[^a-zA-Z0-9_.-]+/g, "-").replace(/^-|-$/g, "").slice(0, 96) || "chat";
}

function rawPreview(rawFrames: string[]): string {
  return rawFrames.slice(0, 4).join(" | ").slice(0, 1000);
}

async function chatTurn(args: {
  threadId: string;
  appId: string;
  messages?: unknown[];
  resume?: { token: string; value: unknown };
  signal: AbortSignal;
}): Promise<{
  chunks: any[];
  deployHash?: string;
  deployOutput?: any;
  survey?: { token: string };
  toolLog: string[];
  errors: string[];
  rawSsePath: string;
  rawPreview: string;
}> {
  const body = args.resume
    ? { json: { id: args.threadId, appId: args.appId, resume: args.resume } }
    : { json: { id: args.threadId, appId: args.appId, messages: args.messages ?? [] } };

  const res = await fetch(`${BUILDER_URL}/_zs/v1/chat`, {
    method: "POST",
    headers: {
      accept: "text/event-stream",
      "content-type": "application/json",
    },
    body: JSON.stringify(body),
    signal: args.signal,
  });
  if (!res.ok) {
    throw new Error(`chat failed (${res.status}): ${await res.text()}`);
  }

  const sse = await readSseChunks(res);
  const chunks = sse.chunks;
  await mkdir(LOG_DIR, { recursive: true });
  const rawSsePath = resolve(
    LOG_DIR,
    `${safeLogName(args.threadId)}-${args.resume ? "resume" : "fresh"}-${Date.now()}.raw.sse`,
  );
  await writeFile(rawSsePath, `${sse.rawFrames.join("\n\n")}\n`);
  const toolNames = new Map<string, string>();
  const toolLog: string[] = [];
  let deployHash: string | undefined;
  let deployOutput: any;
  let survey: { token: string } | undefined;
  const errors: string[] = [...sse.parseErrors];

  for (const chunk of chunks) {
    if (chunk.type === "tool-input-available") {
      toolNames.set(String(chunk.toolCallId), String(chunk.toolName));
      if (["write_file", "edit_file", "execute", "deploy", "task"].includes(String(chunk.toolName))) {
        toolLog.push(`tool:start:${chunk.toolName}`);
      }
    } else if (chunk.type === "tool-output-available") {
      const name = toolNames.get(String(chunk.toolCallId)) ?? "unknown";
      if (["write_file", "edit_file", "execute", "deploy", "task"].includes(name)) {
        toolLog.push(`tool:end:${name}`);
      }
      if (name === "deploy") {
        deployOutput = parseToolOutput(chunk.output);
        if (deployOutput?.deploy_hash) deployHash = String(deployOutput.deploy_hash);
      }
    } else if (chunk.type === "data-diff") {
      toolLog.push(`data:diff:${chunk.data?.path ?? ""}`);
    } else if (chunk.type === "data-critic-round") {
      toolLog.push(`data:critic:${chunk.data?.approved === false ? "blocked" : "approved"}`);
    } else if (chunk.type === "data-survey") {
      survey = { token: String(chunk.data?.token ?? chunk.id) };
      toolLog.push("data:survey");
    } else if (String(chunk.type ?? "").includes("error")) {
      errors.push(JSON.stringify(chunk).slice(0, 1000));
    } else if (chunk.error || chunk.errorText || chunk.message) {
      errors.push(JSON.stringify(chunk).slice(0, 1000));
    }
  }

  return {
    chunks,
    deployHash,
    deployOutput,
    survey,
    toolLog,
    errors,
    rawSsePath,
    rawPreview: rawPreview(sse.rawFrames),
  };
}

async function runBuilderPrompt(prompt: PromptCase, app: { id: string; name: string }) {
  const ac = new AbortController();
  const timeout = setTimeout(() => ac.abort(new Error("per-prompt timeout")), PER_PROMPT_TIMEOUT_MS);
  const threadId = `m0-${prompt.id}-${app.id}`;
  const messages = [userMessage(harnessPrompt(prompt))];
  const allToolLog: string[] = [];
  const allErrors: string[] = [];
  const rawSsePaths: string[] = [];
  const rawPreviews: string[] = [];
  let deployHash: string | undefined;
  let deployOutput: any;

  try {
    let turn = await chatTurn({
      threadId,
      appId: app.id,
      messages,
      signal: ac.signal,
    });
    allToolLog.push(...turn.toolLog);
    allErrors.push(...turn.errors);
    rawSsePaths.push(turn.rawSsePath);
    if (turn.rawPreview) rawPreviews.push(turn.rawPreview);
    deployHash = turn.deployHash;
    deployOutput = turn.deployOutput;

    for (let i = 0; !deployHash && turn.survey && i < MAX_SURVEY_RESUMES; i++) {
      turn = await chatTurn({
        threadId,
        appId: app.id,
        resume: {
          token: turn.survey.token,
          value: { skipped: true },
        },
        signal: ac.signal,
      });
      allToolLog.push(...turn.toolLog);
      allErrors.push(...turn.errors);
      rawSsePaths.push(turn.rawSsePath);
      if (turn.rawPreview) rawPreviews.push(turn.rawPreview);
      deployHash = turn.deployHash;
      deployOutput = turn.deployOutput;
    }
  } finally {
    clearTimeout(timeout);
  }

  if (deployOutput?.blocked) {
    throw new Error(`deploy blocked: ${JSON.stringify(deployOutput).slice(0, 1200)}`);
  }
  const rawSuffix = rawSsePaths.length > 0
    ? `; raw_sse=${rawSsePaths.join(",")}`
    : "";
  if (allErrors.length > 0) {
    throw new Error(`agent stream error: ${allErrors.join(" | ")}${rawSuffix}`);
  }
  if (allToolLog.length === 0) {
    const preview = rawPreviews.length > 0 ? `; raw_preview=${rawPreviews.join(" || ")}` : "";
    throw new Error(`agent produced an empty stream before build/deploy${rawSuffix}${preview}`);
  }
  if (!deployHash) {
    const controlHash = await getDeployHash(app.id);
    if (controlHash) deployHash = controlHash;
  }
  if (!deployHash) {
    throw new Error(`agent did not produce a deploy hash; tools=${allToolLog.join(",")}${rawSuffix}`);
  }

  return { deployHash, toolLog: allToolLog };
}

async function assertReachable(page: Page, appName: string, apiKey?: string): Promise<{
  url: string;
  status: number;
  bodyExcerpt: string;
}> {
  const url = `${GATEWAY_URL}/apps/${encodeURIComponent(appName)}/`;
  const deadline = Date.now() + REACHABLE_TIMEOUT_MS;
  let lastStatus = 0;
  let lastExcerpt = "";

  while (Date.now() < deadline) {
    let response = await page.goto(url, {
      waitUntil: "domcontentloaded",
      timeout: 30_000,
    });
    let status = response?.status() ?? 0;
    if ((status === 401 || status === 403) && apiKey) {
      await page.setExtraHTTPHeaders({ "X-Api-Key": apiKey });
      response = await page.goto(url, { waitUntil: "domcontentloaded", timeout: 30_000 });
      status = response?.status() ?? 0;
      await page.setExtraHTTPHeaders({});
    }

    await page.waitForLoadState("networkidle", { timeout: 10_000 }).catch(() => {});
    const text = (await page.locator("body").innerText({ timeout: 10_000 }).catch(() => "")).trim();
    const excerpt = text.replace(/\s+/g, " ").slice(0, 500);
    lastStatus = status;
    lastExcerpt = excerpt;

    if (
      status >= 200 &&
      status < 400 &&
      excerpt !== "" &&
      !excerpt.toLowerCase().includes("your app starts here")
    ) {
      return { url, status, bodyExcerpt: excerpt };
    }

    await page.waitForTimeout(2_000);
  }

  throw new Error(
    `live fetch did not become reachable for ${url} within ${REACHABLE_TIMEOUT_MS}ms; ` +
    `last_status=${lastStatus}; body=${JSON.stringify(lastExcerpt)}`,
  );
}

test.describe("M0 exit gate - real Builder prompt to deployed app", () => {
  test("fixed prompt set reaches the live gateway", async ({ page }, testInfo) => {
    const allPrompts = await loadPrompts();
    const prompts = selectedPrompts(allPrompts);
    const previous = await loadPreviousOutcomes();
    const resume = process.env.M0_RESUME === "1";
    const minPass = Number(
      process.env.M0_EXPECT_MIN
        ?? (prompts.length === allPrompts.length ? 7 : 0),
    );
    const timeoutBudget = Math.max(180_000, prompts.length * (PER_PROMPT_TIMEOUT_MS + 90_000));
    testInfo.setTimeout(timeoutBudget);

    console.log(`[m0] prompts=${prompts.length}/${allPrompts.length} min_pass=${minPass} per_prompt_timeout_ms=${PER_PROMPT_TIMEOUT_MS}`);
    console.log(`[m0] endpoints builder=${BUILDER_URL} control=${CONTROL_URL} sandbox=${SANDBOX_URL} gateway=${GATEWAY_URL}`);

    const [builderUp, controlUp, sandboxUp, gatewayUp] = await Promise.all([
      probe(BUILDER_URL),
      probe(`${CONTROL_URL}/health`),
      probe(`${SANDBOX_URL}/health`),
      probe(`${GATEWAY_URL}/health`),
    ]);
    expect(builderUp, `builder dev server reachable at ${BUILDER_URL}`).toBe(true);
    expect(controlUp, `control plane reachable at ${CONTROL_URL}`).toBe(true);
    expect(sandboxUp, `sandbox controller reachable at ${SANDBOX_URL}`).toBe(true);
    expect(gatewayUp, `gateway reachable at ${GATEWAY_URL}`).toBe(true);

    let passed = 0;
    let failed = 0;
    let skipped = 0;
    const ran: string[] = [];

    for (const [index, prompt] of prompts.entries()) {
      if (resume && previous[prompt.id]?.status === "pass") {
        skipped += 1;
        passed += 1;
        console.log(`[m0][${index + 1}/${prompts.length}] SKIP ${prompt.id} already passed (${previous[prompt.id].url ?? "no url"})`);
        continue;
      }

      const started = Date.now();
      ran.push(prompt.id);
      console.log(`[m0][${index + 1}/${prompts.length}] START ${prompt.id}: ${prompt.prompt}`);
      console.log(`[m0][${index + 1}/${prompts.length}] criteria: ${prompt.criteria}`);

      let app: { id: string; name: string; api_key?: string } | undefined;
      try {
        app = await createControlApp(prompt);
        console.log(`[m0][${prompt.id}] app id=${app.id} name=${app.name}`);
        const built = await runBuilderPrompt(prompt, app);
        console.log(`[m0][${prompt.id}] deploy_hash=${built.deployHash}`);
        console.log(`[m0][${prompt.id}] tools=${built.toolLog.slice(-30).join(",")}`);
        const live = await assertReachable(page, app.name, app.api_key);
        console.log(`[m0][${prompt.id}] reachable status=${live.status} url=${live.url}`);
        console.log(`[m0][${prompt.id}] body=${JSON.stringify(live.bodyExcerpt)}`);

        passed += 1;
        await saveOutcome({
          id: prompt.id,
          title: prompt.title,
          status: "pass",
          appId: app.id,
          appName: app.name,
          deployHash: built.deployHash,
          url: live.url,
          reachableStatus: live.status,
          bodyExcerpt: live.bodyExcerpt,
          durationMs: Date.now() - started,
          ranAt: new Date().toISOString(),
        });
        console.log(`[m0][${prompt.id}] PASS duration_ms=${Date.now() - started}`);
      } catch (err) {
        failed += 1;
        const message = err instanceof Error ? err.message : String(err);
        console.log(`[m0][${prompt.id}] FAIL ${message}`);
        await saveOutcome({
          id: prompt.id,
          title: prompt.title,
          status: "fail",
          appId: app?.id,
          appName: app?.name,
          error: message,
          durationMs: Date.now() - started,
          ranAt: new Date().toISOString(),
        });
      }
    }

    console.log(`[m0] ran=${ran.join(",") || "(none)"} skipped=${skipped}`);
    console.log(`[m0] PASS ${passed}/${prompts.length} (full-set denominator ${allPrompts.length}; failed=${failed}; skipped=${skipped})`);
    expect(passed, `M0 gate pass count across selected prompts`).toBeGreaterThanOrEqual(minPass);
  });
});
