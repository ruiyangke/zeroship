/*
 * Compile-time regression tests for Toggle / Toggle.Group.
 *
 * This file exists ONLY to assert TypeScript-level contracts via
 * `@ts-expect-error`. The strict-mode `tsup`/`tsc` build that emits the
 * package's `.d.ts` files includes this file, so any of these comments
 * that stop being errors (because the surface relaxed) become BUILD
 * FAILURES — exactly the regression hook we want.
 *
 * No runtime code; the module exports nothing.
 *
 *   - Item 2: `<Toggle.Group role="…">` is rejected — the public
 *     `ToggleGroupProps` Omits `role` so consumers cannot accidentally
 *     override the locked `role="toolbar"` attribute.
 *
 *   - Item 3: single-mode (no `multiple`) `value` is the SCALAR, not an
 *     array; passing an array fails. multiple-mode `value` is the ARRAY,
 *     not a scalar; passing a scalar fails.
 *
 *   - Item 7: `<Toggle<"day" | "week"> value="month">` fails — the
 *     `<Value>` generic narrows the literal-string set.
 *
 * No imports of React or runtime values — pure types.
 */
/* eslint-disable @typescript-eslint/no-unused-vars */
import { createElement } from "react";
import { Toggle } from "./Toggle";

// ─── Item 2: role lock ─────────────────────────────────────────────────
// The runtime stamps `role="toolbar"`; the type must reject a consumer
// passing `role` so the lock is enforced statically.
function _roleOmitRegression() {
  return (
    // @ts-expect-error — `role` is Omit'd from ToggleGroupProps.
    createElement(Toggle.Group, { role: "radiogroup" })
  );
}

// Sanity check: a stock Toggle.Group with NO `role` prop compiles.
function _stockToggleGroupCompiles() {
  return createElement(Toggle.Group, { "aria-label": "x" });
}

// ─── Item 3: discriminated single/multiple value type ──────────────────
function _singleScalarValueCompiles() {
  return createElement(Toggle.Group, {
    value: "day",
    defaultValue: "week",
    onValueChange: (v: string | undefined) => v,
  });
}

function _singleArrayValueRejected() {
  return createElement(
    Toggle.Group,
    // @ts-expect-error — single-mode expects a scalar, not an array.
    { value: ["day"] },
  );
}

function _multipleArrayValueCompiles() {
  return createElement(Toggle.Group, {
    multiple: true,
    value: ["bold", "italic"],
    defaultValue: ["italic"],
    onValueChange: (v: string[]) => v,
  });
}

function _multipleScalarValueRejected() {
  return createElement(Toggle.Group, {
    multiple: true,
    // @ts-expect-error — multiple-mode expects an array, not a scalar.
    value: "bold",
  });
}

// ─── Item 7: ToggleProps<Value> generic narrows the value literal ──────
function _toggleValueLiteralCompiles() {
  return createElement(Toggle<"day" | "week">, { value: "day" });
}

function _toggleValueLiteralRejected() {
  return createElement(
    Toggle<"day" | "week">,
    // @ts-expect-error — "month" is not assignable to "day" | "week".
    { value: "month" },
  );
}
