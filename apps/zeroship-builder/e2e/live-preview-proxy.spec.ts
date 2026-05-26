import { test, expect } from "@playwright/test";

import {
  DEFAULT_PREVIEW_PORT,
  DEV_SANDBOX_USER_ID,
  ZeroshipSandboxBackend,
  getOrCreateSandboxFor,
} from "../src/server/internal/sandbox-backend";

const SANDBOX_URL = process.env.SANDBOX_URL ?? "http://localhost:9091";
const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";
const CONTROL_KEY = process.env.CONTROL_KEY ?? "dev-master-key";
const STEP_TIMEOUT = 180_000;

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

async function createControlApp(name: string): Promise<{ id: string; name: string }> {
  const res = await fetch(`${CONTROL_URL}/api/apps`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CONTROL_KEY}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ name, plan_id: "free" }),
  });
  if (!res.ok) {
    throw new Error(`create app failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as { id: string; name: string };
}

async function deleteControlApp(id: string): Promise<void> {
  await fetch(`${CONTROL_URL}/api/apps/${encodeURIComponent(id)}`, {
    method: "DELETE",
    headers: { authorization: `Bearer ${CONTROL_KEY}` },
  }).catch(() => {});
}

async function startRoundTripServer(args: {
  appId: string;
  nonce: string;
  port?: number;
}): Promise<ZeroshipSandboxBackend> {
  const port = args.port ?? DEFAULT_PREVIEW_PORT;
  const sandbox = await getOrCreateSandboxFor(`live-preview-${args.appId}`, {
    userId: DEV_SANDBOX_USER_ID,
    projectSourceId: args.appId,
  });
  const backend = new ZeroshipSandboxBackend({
    id: sandbox.id,
    userId: sandbox.userId,
  });

  const start = await backend.execute([
    "if [ -f live-preview-roundtrip.pid ]; then",
    "  OLD_PID=\"$(cat live-preview-roundtrip.pid 2>/dev/null || true)\"",
    "  if [ -n \"$OLD_PID\" ]; then kill \"$OLD_PID\" 2>/dev/null || true; fi",
    "fi",
    "cat > live-preview-roundtrip.pl <<'PERL'",
    "use strict;",
    "use warnings;",
    "$SIG{PIPE} = 'IGNORE';",
    "use IO::Socket::INET;",
    "my $nonce = $ENV{LIVE_PREVIEW_NONCE} // '';",
    "my $port = int($ENV{LIVE_PREVIEW_PORT} // 5173);",
    "my $server = IO::Socket::INET->new(",
    "  LocalAddr => '0.0.0.0',",
    "  LocalPort => $port,",
    "  Proto => 'tcp',",
    "  Listen => 10,",
    "  Reuse => 1,",
    ") or die \"listen failed: $!\";",
    "while (my $client = $server->accept()) {",
    "  my $request = <$client>;",
    "  if (!defined($request) || $request eq '') { close $client; next; }",
    "  my $target = '/';",
    "  $target = $1 if $request =~ /^\\S+\\s+(\\S+)/;",
    "  while (defined(my $line = <$client>)) {",
    "    last if $line =~ /^\\r?\\n$/;",
    "  }",
    "  my $body = \"LIVE_PREVIEW_ROUNDTRIP:$nonce:$target\";",
    "  print $client \"HTTP/1.1 200 OK\\r\\n\";",
    "  print $client \"Content-Type: text/plain\\r\\n\";",
    "  print $client \"X-Live-Preview-Roundtrip: $nonce\\r\\n\";",
    "  print $client \"Content-Length: \" . length($body) . \"\\r\\n\\r\\n\";",
    "  print $client $body;",
    "  close $client;",
    "}",
    "PERL",
    `LIVE_PREVIEW_NONCE=${shellQuote(args.nonce)} LIVE_PREVIEW_PORT=${port} nohup perl live-preview-roundtrip.pl > live-preview-roundtrip.log 2>&1 < /dev/null &`,
    "echo \"$!\" > live-preview-roundtrip.pid",
  ].join("\n"));
  expect(start.exitCode).toBe(0);

  return backend;
}

async function stopRoundTripServer(backend: ZeroshipSandboxBackend): Promise<void> {
  await backend.execute([
    "if [ -f live-preview-roundtrip.pid ]; then",
    "  PID=\"$(cat live-preview-roundtrip.pid 2>/dev/null || true)\"",
    "  if [ -n \"$PID\" ]; then kill \"$PID\" 2>/dev/null || true; fi",
    "fi",
  ].join("\n")).catch(() => {});
}

test.describe("live sandbox preview", () => {
  test("builder proxy round-trips through controller preview proxy to an in-sandbox server", async ({ request }, testInfo) => {
    test.skip(
      !(await probe(`${SANDBOX_URL}/health`)),
      `sandbox controller unreachable at ${SANDBOX_URL}`,
    );
    testInfo.setTimeout(STEP_TIMEOUT);

    const appId = `live-preview-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const nonce = `nonce-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const backend = await startRoundTripServer({ appId, nonce });
    const path = `/api/preview/${encodeURIComponent(appId)}/${DEFAULT_PREVIEW_PORT}/roundtrip?nonce=${encodeURIComponent(nonce)}`;

    try {
      let captured: { status: number; body: string; header: string } | null = null;
      await expect.poll(async () => {
        const res = await request.get(path, { failOnStatusCode: false });
        captured = {
          status: res.status(),
          body: await res.text(),
          header: res.headers()["x-live-preview-roundtrip"] ?? "",
        };
        return captured.status === 200 &&
          captured.body.includes(`LIVE_PREVIEW_ROUNDTRIP:${nonce}`) &&
          captured.header === nonce;
      }, {
        intervals: [500, 1000, 1500, 2000],
        timeout: 30_000,
      }).toBe(true);

      console.log(`[live-preview] ${JSON.stringify(captured)}`);
    } finally {
      await stopRoundTripServer(backend);
    }
  });

  test("PreviewCanvas renders an iframe pointed at the builder preview proxy", async ({ page }, testInfo) => {
    const [sandboxUp, controlUp] = await Promise.all([
      probe(`${SANDBOX_URL}/health`),
      probe(`${CONTROL_URL}/health`),
    ]);
    test.skip(!sandboxUp, `sandbox controller unreachable at ${SANDBOX_URL}`);
    test.skip(!controlUp, `control plane unreachable at ${CONTROL_URL}`);
    testInfo.setTimeout(STEP_TIMEOUT);

    const app = await createControlApp(`live-preview-canvas-${Date.now()}`);
    const nonce = `canvas-${Date.now()}`;
    const backend = await startRoundTripServer({ appId: app.id, nonce });

    try {
      await expect.poll(async () => {
        const res = await page.request.get(
          `/api/preview/${encodeURIComponent(app.id)}/${DEFAULT_PREVIEW_PORT}/`,
          { failOnStatusCode: false },
        );
        const body = await res.text();
        return res.status() === 200 && body.includes(`LIVE_PREVIEW_ROUNDTRIP:${nonce}`);
      }, {
        intervals: [500, 1000, 1500, 2000],
        timeout: 30_000,
      }).toBe(true);

      await page.goto(`/p/${app.id}/preview`);
      const frame = page.getByTestId("preview-frame");
      await expect(frame).toBeVisible({ timeout: 30_000 });
      await expect(frame).toHaveAttribute(
        "src",
        `/api/preview/${encodeURIComponent(app.id)}/${DEFAULT_PREVIEW_PORT}/`,
      );
      await expect(page.frameLocator("[data-testid='preview-frame']").locator("body")).toContainText(
        `LIVE_PREVIEW_ROUNDTRIP:${nonce}`,
      );
    } finally {
      await stopRoundTripServer(backend);
      await deleteControlApp(app.id);
    }
  });
});
