/*
 * Inline Slot helpers — shared across components that support `asChild`.
 *
 * Extracted from Button.tsx (slice 1) so Card, Dialog, AlertDialog, and
 * future components can render-as a single React-element child without
 * dragging in `@radix-ui/react-slot`. The merge semantics here match
 * Radix's:
 *
 *   - `className` concatenates (theirs + ours).
 *   - `style` shallow-merges (ours wins on collisions).
 *   - Event handlers compose — the child's handler runs first; if the
 *     child calls `event.preventDefault()` ours is skipped.
 *   - `ref` fans out via `composeRefs` (the consumer's ref AND any of
 *     our internal refs both land on the same node).
 *   - All other props: ours wins via the `{...theirs, ...ours}` base
 *     spread.
 *
 * Filename starts with an underscore so the directory listing makes it
 * obvious this isn't a public component — it's internal plumbing. The
 * surface re-exports nothing from here.
 */
import {
  cloneElement,
  isValidElement,
  type CSSProperties,
  type MutableRefObject,
  type ReactElement,
  type ReactNode,
  type Ref,
  type SyntheticEvent,
} from "react";

type SlotProps = Record<string, unknown> & {
  children?: ReactNode;
};

function classnames(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

/**
 * Apply a value to a React ref of any flavor (callback, object, null).
 * Used by `composeRefs` to fan a single value out to multiple refs.
 */
export function setRef<T>(ref: Ref<T> | undefined, value: T | null): void {
  if (typeof ref === "function") {
    ref(value);
  } else if (ref && typeof ref === "object") {
    (ref as MutableRefObject<T | null>).current = value;
  }
}

/**
 * Compose multiple React refs into a single callback ref. Necessary
 * when both a consumer-supplied ref AND a Base UI internal ref need to
 * point at the same node (validation registration, focus management).
 */
export function composeRefs<T>(...refs: Array<Ref<T> | undefined>): Ref<T> {
  return (value: T | null) => {
    for (const ref of refs) setRef(ref, value);
  };
}

/**
 * Merge our props onto the existing props of a child element.
 *
 * Spread order is `{ ...theirs, ...ours }` — ours wins by default. For
 * the three keys with composition semantics (className / style /
 * onXxx), we replace the base merge result with the composed value.
 */
export function mergeProps(
  ours: Record<string, unknown>,
  theirs: Record<string, unknown>,
): Record<string, unknown> {
  const merged: Record<string, unknown> = { ...theirs, ...ours };

  for (const key of Object.keys(ours)) {
    const oursValue = ours[key];
    const theirsValue = theirs[key];

    if (key === "className" && typeof oursValue === "string" && typeof theirsValue === "string") {
      merged[key] = classnames(theirsValue, oursValue);
    } else if (key === "style" && oursValue && theirsValue) {
      merged[key] = { ...(theirsValue as CSSProperties), ...(oursValue as CSSProperties) };
    } else if (/^on[A-Z]/.test(key) && typeof oursValue === "function" && typeof theirsValue === "function") {
      merged[key] = (event: SyntheticEvent) => {
        (theirsValue as (e: SyntheticEvent) => void)(event);
        if (!event.defaultPrevented) {
          (oursValue as (e: SyntheticEvent) => void)(event);
        }
      };
    }
  }

  return merged;
}

/**
 * Render the single child element with our props merged in. If
 * `children` isn't a valid React element, returns null (the consumer
 * passed something illegal like a string under `asChild`).
 */
export function Slot({ children, ...props }: SlotProps) {
  if (!isValidElement(children)) return null;
  const child = children as ReactElement<Record<string, unknown>> & {
    ref?: Ref<unknown>;
  };
  const merged = mergeProps(
    props as Record<string, unknown>,
    child.props as Record<string, unknown>,
  );
  if ((props as { ref?: Ref<unknown> }).ref || child.ref) {
    merged.ref = composeRefs(
      (props as { ref?: Ref<unknown> }).ref,
      child.ref,
    );
  }
  return cloneElement(child, merged);
}
