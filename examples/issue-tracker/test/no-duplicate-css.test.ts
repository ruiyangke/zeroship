import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { cn } from "../src/ui/cn";
import { Pagination } from "../src/ui/Pagination";

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
    expect(cn("shadow-none", "shadow-popup")).toBe("shadow-popup");
    expect(cn("duration-fast", "duration-base")).toBe("duration-base");
    expect(cn("field-edge", "shadow-none")).toBe("shadow-none");
    expect(cn("before:content-empty", "before:content-placeholder")).toBe(
      "before:content-placeholder",
    );

    const pagination = document.createElement("div");
    pagination.innerHTML = renderToStaticMarkup(
      createElement(Pagination, {
        page: 1,
        pageSize: 25,
        total: 60,
        onPageChange: () => {},
      }),
    );
    const previous = pagination.querySelector('[aria-label="Go to previous page"]');
    expect(previous?.classList).toContain("h-7");
    expect(previous?.classList).toContain("px-3");
    expect(previous?.classList).not.toContain("h-6");
    expect(previous?.classList).not.toContain("px-2");
  });
});
