import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

import { cn } from "../src/ui/cn";

describe("app-owned styling", () => {
  it("does not recreate the removed app stylesheet", () => {
    expect(
      existsSync(resolve(process.cwd(), "src/styles.css")),
      "keep app-owned presentation with its React markup as Tailwind utilities",
    ).toBe(false);
  });

  it("loads the theme once and resolves utility conflicts at component boundaries", () => {
    const main = readFileSync(resolve(process.cwd(), "src/main.tsx"), "utf8");
    expect(main).not.toContain('import "./styles.css"');

    expect(
      cn("h-6 px-2 text-ink", ["h-7", false, "px-3 text-danger"]),
    ).toBe("h-7 px-3 text-danger");
  });
});
