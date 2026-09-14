/*
 * Type-only regression test for Select's overload-based discriminated
 * union. Pre-fix: a single union signature locks `Value` from the
 * `value` prop before reading `multiple`, so a multi-mode story shape
 * (`useState<string[]>(...)` + `<Select multiple value={value}>`)
 * infers `Value=string[]` and TypeScript demands `readonly string[][]`.
 *
 * Post-fix: overloads let TS pick the SelectMultipleProps branch with
 * `Value=string`, accepting `string[]`. The `// @ts-expect-error` lines
 * below assert that invalid combinations still error — without them a
 * regression that loosens types too far would slip through unflagged.
 *
 * Run this file via `pnpm exec tsc -p tsconfig.json --noEmit` (the
 * standard UI tsconfig already includes `src/**`).
 */
import { Select } from "./Select";

/* ─── Multi-mode (the regression case from the Slice 6 review) ─────── */
export function multiModeOk(value: string[]) {
  // `useState<string[]>` produces `value: string[]`; with `multiple` the
  // overload picks SelectMultipleProps<string>. Pre-fix this errored.
  return (
    <Select
      multiple
      value={value}
      onValueChange={(next) => {
        // Multi-branch onValueChange callback receives `string[]`.
        const _: string[] = next;
        void _;
      }}
      placeholder="Pick fruits"
    />
  );
}

/* ─── Single-mode default ──────────────────────────────────────────── */
export function singleModeOk(value: string | null) {
  return (
    <Select
      value={value}
      onValueChange={(next) => {
        // Single-branch callback receives `string | null` (Base UI emits
        // null on clear-paths).
        const _: string | null = next;
        void _;
      }}
      placeholder="Pick one"
    />
  );
}

/* ─── Invalid shapes — these MUST still error ──────────────────────── *
 *
 * The @ts-expect-error directives below suppress the expected errors.
 * If a future refactor loosens types too far and these stop erroring,
 * `tsc --noEmit` flags the unused directives — that's the regression
 * signal. The errors land on the prop assignment line (not the JSX
 * opening tag), so the directive precedes that line.
 */
export function invalidMultiCallbackShape() {
  return (
    <Select<string>
      multiple
      value={["apple"]}
      // @ts-expect-error — multi-mode onValueChange must accept Value[],
      //                    not a scalar. A scalar would drop data.
      onValueChange={(_v: string) => {
        void _v;
      }}
    />
  );
}

export function invalidSingleCallbackShape() {
  return (
    <Select<string>
      value="apple"
      // @ts-expect-error — single-mode onValueChange must accept
      //                    `Value | null`. Narrowing to `number` is
      //                    nonsense; null narrowing to `Value` is unsound.
      onValueChange={(_v: number) => {
        void _v;
      }}
    />
  );
}
