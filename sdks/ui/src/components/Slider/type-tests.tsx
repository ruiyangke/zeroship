/*
 * Type-only regression test for Slider's overload-based discriminated
 * union. Pre-fix: a single `SliderProps` union signature meant
 * TypeScript couldn't narrow `onValueChange`'s parameter from the
 * value/defaultValue shape, so `<Slider value={[20, 60]} onValueChange=
 * {(next) => …}>` left `next` typed implicit-any.
 *
 * Post-fix: overloads let TS try the Single branch first, fall through
 * to Range when `value` / `defaultValue` is an array, and narrow the
 * callback to `(number) => void` vs `(number[]) => void` at the call
 * site.
 *
 * Mirrors `sdks/ui/src/components/Select/type-tests.tsx` (Slice 6 fix).
 * Run via `pnpm exec tsc -p tsconfig.json --noEmit` — the standard UI
 * tsconfig already includes `src/**`.
 */
import { Slider } from "./Slider";

/* ─── Single-mode default — onValueChange receives `number` ────────── */
export function singleModeOk(value: number) {
  return (
    <Slider
      value={value}
      onValueChange={(next) => {
        // Single branch: scalar callback.
        const _: number = next;
        void _;
      }}
      min={0}
      max={100}
    />
  );
}

/* ─── Range mode (controlled) — callback receives `number[]` ────────── */
export function rangeModeOk(value: readonly number[]) {
  return (
    <Slider
      value={value}
      onValueChange={(next) => {
        // Range branch: array callback.
        const _: number[] = next;
        void _;
      }}
      min={0}
      max={100}
    />
  );
}

/* ─── Range mode (uncontrolled defaultValue only) ──────────────────── */
export function rangeUncontrolledOk(defaultValue: readonly number[]) {
  return (
    <Slider
      defaultValue={defaultValue}
      onValueChange={(next) => {
        const _: number[] = next;
        void _;
      }}
      min={0}
      max={100}
    />
  );
}

/* ─── Invalid shapes — these MUST still error ──────────────────────── *
 *
 * The @ts-expect-error directives below suppress the expected errors.
 * If a future refactor loosens types too far and these stop erroring,
 * `tsc --noEmit` flags the unused directive — that's the regression
 * signal.
 */
export function invalidSingleCallbackShape() {
  return (
    <Slider
      value={50}
      // @ts-expect-error — single-mode onValueChange receives `number`,
      //                    not `string`. A string would round-trip nonsense
      //                    through Base UI's numeric state machine.
      onValueChange={(_v: string) => {
        void _v;
      }}
      min={0}
      max={100}
    />
  );
}

export function invalidRangeCallbackShape() {
  return (
    <Slider
      value={[20, 60]}
      // @ts-expect-error — range-mode onValueChange must accept
      //                    `number[]`, not a scalar. A scalar would drop
      //                    the second thumb's value silently.
      onValueChange={(_v: number) => {
        void _v;
      }}
      min={0}
      max={100}
    />
  );
}
