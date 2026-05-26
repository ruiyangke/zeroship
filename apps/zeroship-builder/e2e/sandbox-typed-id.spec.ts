import { test, expect } from "@playwright/test";

import {
  DEV_SANDBOX_USER_ID,
  ZeroshipSandboxBackend,
  deriveSandboxProjectId,
  getOrCreateSandboxFor,
} from "../src/server/_sandbox_backend";

const STEP_TIMEOUT = 120_000;
const PROJECT_SOURCE = "00000000-0000-7000-8000-0000000000b1";
const THREAD_ID = "b1-sandbox-typed-id-regression";

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

test.describe("sandbox typed-id integration", () => {
  test("builder backend creates, writes, reads, and execs with typed owner ids", async ({}, testInfo) => {
    test.skip(
      !process.env.SANDBOX_URL,
      "SANDBOX_URL unset; set it to the live sandbox controller, e.g. http://localhost:9091",
    );
    testInfo.setTimeout(STEP_TIMEOUT);

    const sandbox = await getOrCreateSandboxFor(THREAD_ID, {
      userId: DEV_SANDBOX_USER_ID,
      projectSourceId: PROJECT_SOURCE,
    });

    expect(sandbox.id).toMatch(/^sbx_[0-9A-Za-z]{22}$/);
    expect(sandbox.userId).toBe(DEV_SANDBOX_USER_ID);
    expect(sandbox.projectId).toBe(deriveSandboxProjectId(PROJECT_SOURCE));
    expect(sandbox.projectId).toMatch(/^prj_[0-9A-Za-z]{22}$/);

    const backend = new ZeroshipSandboxBackend({
      id: sandbox.id,
      userId: sandbox.userId,
    });
    const path = "b1-typed-id-regression.txt";
    const content = `typed-id regression ${Date.now()}`;

    const write = await backend.write(path, content);
    expect(write.error).toBeUndefined();

    const [download] = await backend.downloadFiles([path]);
    expect(download.error).toBeNull();
    expect(new TextDecoder().decode(download.content!)).toBe(content);

    const exec = await backend.execute(`cat ${shellQuote(path)} && printf '\\nexec-ok'`);
    expect(exec.exitCode).toBe(0);
    expect(exec.output).toContain(content);
    expect(exec.output).toContain("exec-ok");
  });
});
