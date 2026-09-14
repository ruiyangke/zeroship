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
 *     spread — but `undefined` values from `ours` are filtered out
 *     BEFORE the spread so we don't accidentally clobber explicit
 *     props on the child (slice-3 review-fix item 5).
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
import { classnames } from "./_classnames";

type SlotProps = Record<string, unknown> & {
  children?: ReactNode;
};

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
 * React-version-safe access to a child element's ref.
 *
 * React 19 moved the ref onto `element.props.ref` for function
 * components; the legacy `element.ref` property emits a deprecation
 * warning and may return null. This helper checks the new location
 * first so refs forwarded into asChild keep composing under React 19
 * (the workspace catalog pins `react: ^19.2.0`). Slice-3 review-fix
 * item 4.
 */
export function getElementRef<T = unknown>(
  element: ReactElement,
): Ref<T> | undefined {
  const propsRef = (element.props as { ref?: Ref<T> } | null | undefined)?.ref;
  if (propsRef !== undefined) return propsRef;
  // Fallback for any React 18 peer-dep callers still in the build.
  return (element as unknown as { ref?: Ref<T> }).ref;
}

/**
 * Merge our props onto the existing props of a child element.
 *
 * Spread order is `{ ...theirs, ...oursDefined }` — ours wins by
 * default. For the three keys with composition semantics (className /
 * style / onXxx), we replace the base merge result with the composed
 * value. Undefined values in `ours` are filtered before the spread so
 * `ours.tabIndex === undefined` does NOT overwrite a `tabIndex={0}`
 * explicitly set on the child (slice-3 review-fix item 5).
 */
export function mergeProps(
  ours: Record<string, unknown>,
  theirs: Record<string, unknown>,
): Record<string, unknown> {
  const oursDefined: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(ours)) {
    if (value !== undefined) oursDefined[key] = value;
  }

  const merged: Record<string, unknown> = { ...theirs, ...oursDefined };

  for (const key of Object.keys(oursDefined)) {
    const oursValue = oursDefined[key];
    const theirsValue = theirs[key];

    if (
      key === "className" &&
      typeof oursValue === "string" &&
      typeof theirsValue === "string"
    ) {
      merged[key] = classnames(theirsValue, oursValue);
    } else if (key === "style" && oursValue && theirsValue) {
      merged[key] = {
        ...(theirsValue as CSSProperties),
        ...(oursValue as CSSProperties),
      };
    } else if (
      /^on[A-Z]/.test(key) &&
      typeof oursValue === "function" &&
      typeof theirsValue === "function"
    ) {
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
  const child = children as ReactElement<Record<string, unknown>>;
  const childRef = getElementRef<unknown>(child);
  const ourRef = (props as { ref?: Ref<unknown> }).ref;
  const merged = mergeProps(
    props as Record<string, unknown>,
    child.props as Record<string, unknown>,
  );
  if (ourRef !== undefined || childRef !== undefined) {
    merged.ref = composeRefs(ourRef, childRef);
  }
  return cloneElement(child, merged);
}
