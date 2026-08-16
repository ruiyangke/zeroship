import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("app-owned styling", () => {
  it("does not recreate the removed app stylesheet", () => {
    expect(
      existsSync(resolve(process.cwd(), "src/styles.css")),
      "keep app-owned presentation with its React markup as Tailwind utilities",
    ).toBe(false);
  });

  it("loads styling only through the theme entrypoint", () => {
    const main = readFileSync(resolve(process.cwd(), "src/main.tsx"), "utf8");
    expect(main).not.toContain('import "./styles.css"');
  });
});
