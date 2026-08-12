import { expect, test, type BrowserContext, type Page } from "@playwright/test";

const uniq = (prefix: string) =>
  `${prefix}-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 5)}`;

async function openCount(page: Page): Promise<number> {
  const txt = await page.locator(".count").innerText();
  const m = txt.match(/(\d+)/);
  return m ? Number(m[1]) : NaN;
}

async function bootedPage(page: Page): Promise<Page> {
  page.on("pageerror", (err) => console.error(`[browser pageerror] ${err.message}`));
  await page.goto("/", { waitUntil: "domcontentloaded", timeout: 30_000 });
  await expect(page.getByRole("heading", { name: "Todos" })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByPlaceholder("Add a task…")).toBeEnabled({ timeout: 45_000 });
  return page;
}

async function addTodo(page: Page, { priority = "high" }: { priority?: "low" | "medium" | "high" } = {}) {
  const title = uniq("e2e");
  await page.getByPlaceholder("Add a task…").fill(title);
  await page.locator(`.prio button[aria-label="${priority} priority"]`).click();
  await page.locator("button.add").click();
  await expect(page.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 10_000 });
  return title;
}

test("initial load does not duplicate bootstrap RPCs", async ({ page }) => {
  const counts = { publicUser: 0, listTodos: 0 };
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/__zeroship/v1/users.public") counts.publicUser += 1;
    if (url.pathname === "/__zeroship/v1/todos.list") counts.listTodos += 1;
  });

  await bootedPage(page);
  await expect(page.locator(".live.on")).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => counts.publicUser, { timeout: 5_000 }).toBe(1);
  await expect.poll(() => counts.listTodos, { timeout: 5_000 }).toBe(1);
  await page.waitForTimeout(900);

  expect(counts.publicUser).toBe(1);
  expect(counts.listTodos).toBe(1);
});

test("todo stream emits one snapshot for one committed create", async ({ page }) => {
  await bootedPage(page);
  const title = uniq("stream");

  const frames = await page.evaluate(async (todoTitle) => {
    const unwrap = (value: unknown): unknown => {
      if (value && typeof value === "object" && "json" in value) {
        return (value as { json: unknown }).json;
      }
      return value;
    };
    const postJson = async (id: string, input: unknown, accept = "application/json") => {
      const res = await fetch(`/__zeroship/v1/${id}`, {
        method: "POST",
        headers: { accept, "content-type": "application/json" },
        body: JSON.stringify(input),
      });
      if (!res.ok) throw new Error(`${id} failed: ${res.status}`);
      return res;
    };

    const userRes = await postJson("users.public", {});
    const user = unwrap(await userRes.json()) as { id: string };

    const controller = new AbortController();
    const streamRes = await fetch("/__zeroship/v1/todos.subscribe", {
      method: "POST",
      headers: { accept: "text/event-stream", "content-type": "application/json" },
      body: JSON.stringify({ userId: user.id }),
      signal: controller.signal,
    });
    if (!streamRes.ok || !streamRes.body) {
      throw new Error(`todos.subscribe failed: ${streamRes.status}`);
    }

    const reader = streamRes.body.getReader();
    const decoder = new TextDecoder();
    const lines: string[] = [];
    let buffered = "";
    const pump = (async () => {
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffered += decoder.decode(value, { stream: true });
          for (;;) {
            const idx = buffered.indexOf("\n");
            if (idx < 0) break;
            const line = buffered.slice(0, idx).trim();
            buffered = buffered.slice(idx + 1);
            if (line) lines.push(line);
          }
        }
      } catch {
        /* abort closes the reader */
      }
    })();

    const waitFor = async (predicate: () => boolean, timeoutMs: number) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (predicate()) return;
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      throw new Error(`timed out waiting for stream frames: ${lines.join("\n")}`);
    };

    await waitFor(() => lines.some((line) => line.startsWith("2:")), 5_000);
    await postJson("todos.create", { userId: user.id, title: todoTitle, priority: "low" });
    await waitFor(() => lines.some((line) => line.includes(todoTitle)), 5_000);
    await new Promise((resolve) => setTimeout(resolve, 700));

    controller.abort();
    await reader.cancel().catch(() => undefined);
    await pump;

    return lines.filter((line) => line.includes(todoTitle));
  }, title);

  expect(frames).toHaveLength(1);
});

test.describe.serial("db-todos UI", () => {
  let context: BrowserContext;
  let page: Page;
  let createdTitle: string;

  test.beforeAll(async ({ browser }) => {
    context = await browser.newContext();
    page = await bootedPage(await context.newPage());
  });

  test.afterAll(async () => {
    await context?.close();
  });

  test("app mounts with heading and enabled composer", async () => {
    await expect(page.getByRole("heading", { name: "Todos" })).toBeVisible();
    await expect(page.getByPlaceholder("Add a task…")).toBeEnabled();
  });

  test("LIVE indicator turns on after SSE connects", async () => {
    await expect(page.locator(".live.on")).toBeVisible({ timeout: 15_000 });
  });

  test("create adds a composer todo that renders", async () => {
    createdTitle = await addTodo(page, { priority: "high" });
    await expect(page.locator(".item", { hasText: createdTitle }).first()).toBeVisible();
  });

  test("toggle done marks the created todo done", async () => {
    const item = page.locator(".item", { hasText: createdTitle }).first();
    await item.locator(".box").click();
    await expect(page.locator(".item.done", { hasText: createdTitle }).first()).toBeVisible({ timeout: 10_000 });
  });

  test("toggle back unmarks the created todo", async () => {
    const item = page.locator(".item", { hasText: createdTitle }).first();
    await item.locator(".box").click();
    await expect(page.locator(".item.done", { hasText: createdTitle })).toHaveCount(0, { timeout: 10_000 });
  });

  test("archive removes the created row from the list", async () => {
    const item = page.locator(".item", { hasText: createdTitle });
    await item.first().hover();
    await item.first().locator('.icon-btn[aria-label="archive"]').click();
    await expect(item).toHaveCount(0, { timeout: 10_000 });
  });

  test("delete soft-deletes and removes the row", async () => {
    const title = await addTodo(page);
    const item = page.locator(".item", { hasText: title });
    await item.first().hover();
    await item.first().locator('.icon-btn[aria-label="delete"]').click();
    await expect(item).toHaveCount(0, { timeout: 10_000 });
  });

  test("demo seeder bulk-creates ten open todos", async () => {
    const before = await openCount(page);
    await page.locator("button.ghost").click();
    await expect
      .poll(() => openCount(page), { timeout: 20_000 })
      .toBeGreaterThanOrEqual(before + 10);
  });

  test("realtime sends a todo created in tab A to tab B", async ({ browser }) => {
    const contextB = await browser.newContext();
    const pageB = await bootedPage(await contextB.newPage());
    try {
      const title = uniq("rt");
      await page.getByPlaceholder("Add a task…").fill(title);
      await page.locator("button.add").click();
      await expect(pageB.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 15_000 });
    } finally {
      await contextB.close();
    }
  });
});

// The APP's own subscribe call, asserted at the network layer.
//
// The gap this closes: nothing here watched the request the application
// actually issues. `todo stream emits one snapshot for one committed create`
// drives its own `fetch` inside `page.evaluate`, which proves the ENDPOINT
// works and says nothing about whether the app calls it. `realtime sends a todo
// created in tab A to tab B` asserts the visible outcome, which a refetch on
// focus or a poll would also satisfy. So the app could stop subscribing
// entirely and this suite would stay green.
//
// That is not hypothetical. Shipped behaviour on 2026-08-12 was a subscription
// that returned 200 text/event-stream, delivered its initial snapshot, and then
// never emitted again: cross-isolate delivery was unimplemented, so a write
// served by a different V8 isolate was simply lost. Every signal short of "a
// second frame arrives" looked healthy.
test("the app subscribes over SSE and the stream carries a later write", async ({ page }) => {
  const subscribeCalls: string[] = [];
  let subscribeStatus: number | null = null;
  let subscribeContentType: string | null = null;

  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/__zeroship/v1/todos.subscribe") {
      subscribeCalls.push(request.method());
    }
  });
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/__zeroship/v1/todos.subscribe") {
      subscribeStatus = response.status();
      subscribeContentType = response.headers()["content-type"] ?? null;
    }
  });

  await bootedPage(page);

  // The app issued it, exactly once, and the server accepted it as a stream.
  await expect.poll(() => subscribeCalls.length, { timeout: 20_000 }).toBe(1);
  expect(subscribeCalls[0]).toBe("POST");
  await expect.poll(() => subscribeStatus, { timeout: 20_000 }).toBe(200);
  expect(subscribeContentType).toContain("text/event-stream");

  // The UI's own readiness signal, which is driven by the stream opening.
  await expect(page.locator(".live.on")).toBeVisible({ timeout: 15_000 });

  // LOAD-BEARING: a write made AFTER the stream is open must reach it. Without
  // this the test passes on a subscription that opens and then goes deaf, which
  // is precisely the shipped defect described above. The write goes through a
  // second browser context so it is a separate request the gateway is free to
  // route to any worker, and the assertion is on the FIRST page, which never
  // reloads.
  const writer = await page.context().browser()!.newContext();
  try {
    const writerPage = await bootedPage(await writer.newPage());
    const title = uniq("sse");
    await writerPage.getByPlaceholder("Add a task…").fill(title);
    await writerPage.locator("button.add").click();
    await expect(writerPage.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 10_000 });

    await expect(page.locator(".item", { hasText: title }).first()).toBeVisible({ timeout: 20_000 });
    expect(subscribeCalls.length).toBe(1); // delivered by the stream, not a re-subscribe
  } finally {
    await writer.close();
  }
});
