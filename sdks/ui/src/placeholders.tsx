/**
 * Temporary native-HTML pass-throughs for component names the rest of the
 * monorepo imports from `@zeroship/ui`.
 *
 * These exist so consumers (currently only `apps/zeroship-builder`) keep
 * building during the design-system rebuild. Each placeholder is deleted
 * from this file as the real implementation lands under
 * `src/components/<name>/`.
 *
 * Slice 3 removed Card and Dialog (real components now live in
 * `src/components/Card/` and `src/components/Dialog/`). AlertDialog is
 * new — also under `src/components/AlertDialog/`. Only Badge remains
 * as a placeholder.
 */
import type {
  ComponentPropsWithoutRef,
  ReactNode,
} from "react";

type SpanProps = ComponentPropsWithoutRef<"span">;

// ───── Badge ─────────────────────────────────────────────────────────────────

export interface BadgeProps extends SpanProps {
  tone?: string;
  children?: ReactNode;
}

export function Badge({ tone: _tone, children, ...props }: BadgeProps) {
  return <span {...props}>{children}</span>;
}
