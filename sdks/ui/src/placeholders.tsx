/**
 * Temporary native-HTML pass-throughs for component names the rest of the
 * monorepo imports from `@zeroship/ui`.
 *
 * These exist so consumers (currently only `apps/zeroship-builder`) keep
 * building during the Apple-HIG rebuild. Each placeholder is deleted from
 * this file as the real, HIG-styled implementation lands under
 * `src/components/<name>/`.
 *
 * Props that the real components will eventually carry (variant, tone,
 * loading, label, footer, interactive, ...) are accepted here and silently
 * dropped — the placeholder renders only the DOM-safe subset, so visuals
 * are flat-and-ugly during the rebuild but type-checking stays green for
 * consumers that already use the eventual API.
 *
 * When this file is empty, the migration is complete and the file gets
 * deleted along with this comment.
 */
import type {
  ComponentPropsWithoutRef,
  ReactNode,
} from "react";

type DivProps = ComponentPropsWithoutRef<"div">;
type NativeInputProps = ComponentPropsWithoutRef<"input">;
type SpanProps = ComponentPropsWithoutRef<"span">;

// ───── Input ─────────────────────────────────────────────────────────────────

export interface InputProps extends NativeInputProps {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export function Input({
  label: _label,
  hint: _hint,
  error: _error,
  ...props
}: InputProps) {
  return <input {...props} />;
}

// ───── Card ──────────────────────────────────────────────────────────────────

export interface CardProps extends DivProps {
  tone?: string;
  interactive?: boolean;
  children?: ReactNode;
}

export function Card({
  tone: _tone,
  interactive: _interactive,
  children,
  ...props
}: CardProps) {
  return <div {...props}>{children}</div>;
}

// ───── Badge ─────────────────────────────────────────────────────────────────

export interface BadgeProps extends SpanProps {
  tone?: string;
  children?: ReactNode;
}

export function Badge({ tone: _tone, children, ...props }: BadgeProps) {
  return <span {...props}>{children}</span>;
}

// ───── Dialog ────────────────────────────────────────────────────────────────

export interface DialogProps extends Omit<DivProps, "title"> {
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  title?: ReactNode;
  description?: ReactNode;
  trigger?: ReactNode;
  footer?: ReactNode;
  children?: ReactNode;
}

export function Dialog({
  open,
  onOpenChange: _onOpenChange,
  title,
  description,
  trigger,
  footer,
  children,
  ...props
}: DialogProps) {
  if (!open) return <>{trigger ?? null}</>;
  return (
    <div role="dialog" aria-modal="true" {...props}>
      {title ? <h2>{title}</h2> : null}
      {description ? <p>{description}</p> : null}
      {children}
      {footer ? <div>{footer}</div> : null}
    </div>
  );
}
