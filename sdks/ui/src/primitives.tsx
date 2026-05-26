import {
  forwardRef,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type HTMLAttributes,
  type InputHTMLAttributes,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
  type SelectHTMLAttributes,
  type TableHTMLAttributes,
  type TextareaHTMLAttributes,
} from "react";
import { createPortal } from "react-dom";
import clsx from "clsx";

type Tone = "neutral" | "success" | "warn" | "danger" | "info";

export interface SpinnerProps extends HTMLAttributes<HTMLSpanElement> {
  size?: "sm" | "md" | "lg";
}

export function Spinner({ size = "md", className, ...props }: SpinnerProps) {
  return (
    <span
      aria-hidden="true"
      className={clsx("zs-spinner", `zs-spinner--${size}`, className)}
      {...props}
    />
  );
}

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: "primary" | "secondary" | "ghost" | "danger";
  size?: "sm" | "md" | "lg";
  loading?: boolean;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "primary",
    size = "md",
    loading = false,
    disabled,
    children,
    className,
    type = "button",
    ...props
  },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      className={clsx(
        "zs-button",
        `zs-button--${variant}`,
        `zs-button--${size}`,
        loading && "zs-button--loading",
        className,
      )}
      aria-busy={loading || undefined}
      {...props}
    >
      {loading && <Spinner size="sm" />}
      <span className="zs-button__label">{children}</span>
    </button>
  );
});

interface FieldChromeProps {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  id?: string;
  children: (fieldId: string, describedBy: string | undefined) => ReactNode;
  className?: string;
}

function FieldChrome({ label, hint, error, id, children, className }: FieldChromeProps) {
  const generatedId = useId();
  const fieldId = id ?? generatedId;
  const hintId = hint ? `${fieldId}-hint` : undefined;
  const errorId = error ? `${fieldId}-error` : undefined;
  const describedBy = [hintId, errorId].filter(Boolean).join(" ") || undefined;

  return (
    <label className={clsx("zs-field", className)} htmlFor={fieldId}>
      {label && <span className="zs-field__label">{label}</span>}
      {children(fieldId, describedBy)}
      {hint && !error && (
        <span className="zs-field__hint" id={hintId}>
          {hint}
        </span>
      )}
      {error && (
        <span className="zs-field__error" id={errorId}>
          {error}
        </span>
      )}
    </label>
  );
}

export interface InputProps extends InputHTMLAttributes<HTMLInputElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { label, hint, error, className, id, ...props },
  ref,
) {
  return (
    <FieldChrome label={label} hint={hint} error={error} id={id}>
      {(fieldId, describedBy) => (
        <input
          ref={ref}
          id={fieldId}
          aria-invalid={error ? true : undefined}
          aria-describedby={describedBy}
          className={clsx("zs-input", className)}
          {...props}
        />
      )}
    </FieldChrome>
  );
});

export interface TextareaProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaProps>(function Textarea(
  { label, hint, error, className, id, ...props },
  ref,
) {
  return (
    <FieldChrome label={label} hint={hint} error={error} id={id}>
      {(fieldId, describedBy) => (
        <textarea
          ref={ref}
          id={fieldId}
          aria-invalid={error ? true : undefined}
          aria-describedby={describedBy}
          className={clsx("zs-textarea", className)}
          {...props}
        />
      )}
    </FieldChrome>
  );
});

export interface SelectProps extends SelectHTMLAttributes<HTMLSelectElement> {
  label?: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
}

export const Select = forwardRef<HTMLSelectElement, SelectProps>(function Select(
  { label, hint, error, className, id, children, ...props },
  ref,
) {
  return (
    <FieldChrome label={label} hint={hint} error={error} id={id}>
      {(fieldId, describedBy) => (
        <span className="zs-select-wrap">
          <select
            ref={ref}
            id={fieldId}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
            className={clsx("zs-select", className)}
            {...props}
          >
            {children}
          </select>
        </span>
      )}
    </FieldChrome>
  );
});

export interface CardProps extends HTMLAttributes<HTMLDivElement> {
  tone?: "neutral" | "accent" | "danger";
  interactive?: boolean;
}

export const Card = forwardRef<HTMLDivElement, CardProps>(function Card(
  { tone = "neutral", interactive = false, className, ...props },
  ref,
) {
  return (
    <div
      ref={ref}
      className={clsx(
        "zs-card",
        `zs-card--${tone}`,
        interactive && "zs-card--interactive",
        className,
      )}
      {...props}
    />
  );
});

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: Tone;
}

export function Badge({ tone = "neutral", className, ...props }: BadgeProps) {
  return (
    <span className={clsx("zs-badge", `zs-badge--${tone}`, className)} {...props} />
  );
}

export interface ChipProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  active?: boolean;
  tone?: Tone;
}

export const Chip = forwardRef<HTMLButtonElement, ChipProps>(function Chip(
  { active = false, tone = "neutral", className, type = "button", ...props },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      aria-pressed={active}
      className={clsx(
        "zs-chip",
        `zs-chip--${tone}`,
        active && "zs-chip--active",
        className,
      )}
      {...props}
    />
  );
});

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  className?: string;
}

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  className,
}: DialogProps) {
  const titleId = useId();
  const descriptionId = useId();
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    panel?.focus();

    function onKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") onOpenChange(false);
    }

    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      previous?.focus?.();
    };
  }, [onOpenChange, open]);

  if (!open || typeof document === "undefined") return null;

  return createPortal(
    <div className="zs-dialog" role="presentation">
      <button
        type="button"
        className="zs-dialog__backdrop"
        aria-label="Close dialog"
        onClick={() => onOpenChange(false)}
      />
      <div
        ref={panelRef}
        className={clsx("zs-dialog__panel", className)}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={description ? descriptionId : undefined}
        tabIndex={-1}
      >
        <div className="zs-dialog__header">
          <h2 className="zs-dialog__title" id={titleId}>
            {title}
          </h2>
          {description && (
            <p className="zs-dialog__description" id={descriptionId}>
              {description}
            </p>
          )}
        </div>
        <div className="zs-dialog__body">{children}</div>
        {footer && <div className="zs-dialog__footer">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}

export interface TabsItem {
  value: string;
  label: ReactNode;
  content: ReactNode;
  disabled?: boolean;
}

export interface TabsProps extends HTMLAttributes<HTMLDivElement> {
  items: TabsItem[];
  value?: string;
  defaultValue?: string;
  onValueChange?: (value: string) => void;
}

export function Tabs({
  items,
  value,
  defaultValue,
  onValueChange,
  className,
  ...props
}: TabsProps) {
  const firstEnabled = items.find((item) => !item.disabled)?.value ?? items[0]?.value ?? "";
  const [internalValue, setInternalValue] = useState(defaultValue ?? firstEnabled);
  const selectedValue = value ?? internalValue;
  const baseId = useId();

  const setSelected = useCallback(
    (next: string) => {
      if (value === undefined) setInternalValue(next);
      onValueChange?.(next);
    },
    [onValueChange, value],
  );

  function onKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
    event.preventDefault();
    const enabled = items.filter((item) => !item.disabled);
    const currentIndex = enabled.findIndex((item) => item.value === selectedValue);
    const offset = event.key === "ArrowRight" ? 1 : -1;
    const next = enabled[(currentIndex + offset + enabled.length) % enabled.length];
    if (next) setSelected(next.value);
  }

  return (
    <div className={clsx("zs-tabs", className)} {...props}>
      <div className="zs-tabs__list" role="tablist" onKeyDown={onKeyDown}>
        {items.map((item) => {
          const selected = item.value === selectedValue;
          return (
            <button
              key={item.value}
              type="button"
              role="tab"
              id={`${baseId}-tab-${item.value}`}
              aria-selected={selected}
              aria-controls={`${baseId}-panel-${item.value}`}
              disabled={item.disabled}
              tabIndex={selected ? 0 : -1}
              className="zs-tabs__tab"
              onClick={() => setSelected(item.value)}
            >
              {item.label}
            </button>
          );
        })}
      </div>
      {items.map((item) => {
        const selected = item.value === selectedValue;
        return (
          <div
            key={item.value}
            role="tabpanel"
            id={`${baseId}-panel-${item.value}`}
            aria-labelledby={`${baseId}-tab-${item.value}`}
            hidden={!selected}
            className="zs-tabs__panel"
          >
            {item.content}
          </div>
        );
      })}
    </div>
  );
}

export interface ToastProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  tone?: Tone;
  title?: ReactNode;
  action?: ReactNode;
}

export function Toast({
  tone = "neutral",
  title,
  action,
  children,
  className,
  ...props
}: ToastProps) {
  const role = tone === "danger" || tone === "warn" ? "alert" : "status";
  return (
    <div
      className={clsx("zs-toast", `zs-toast--${tone}`, className)}
      role={role}
      {...props}
    >
      <div className="zs-toast__content">
        {title && <div className="zs-toast__title">{title}</div>}
        {children && <div className="zs-toast__body">{children}</div>}
      </div>
      {action && <div className="zs-toast__action">{action}</div>}
    </div>
  );
}

export interface ToastViewportProps extends HTMLAttributes<HTMLDivElement> {
  children: ReactNode;
}

export function ToastViewport({ children, className, ...props }: ToastViewportProps) {
  return (
    <div className={clsx("zs-toast-viewport", className)} {...props}>
      {children}
    </div>
  );
}

export interface TableColumn<T> {
  key: keyof T | string;
  header: ReactNode;
  cell?: (row: T) => ReactNode;
  align?: "start" | "end";
}

export interface TableProps<T extends Record<string, unknown>>
  extends Omit<TableHTMLAttributes<HTMLTableElement>, "children"> {
  columns: TableColumn<T>[];
  rows: T[];
  getRowKey?: (row: T, index: number) => string;
  empty?: ReactNode;
}

export function Table<T extends Record<string, unknown>>({
  columns,
  rows,
  getRowKey,
  empty = "No records yet.",
  className,
  ...props
}: TableProps<T>) {
  return (
    <div className="zs-table-wrap">
      <table className={clsx("zs-table", className)} {...props}>
        <thead>
          <tr>
            {columns.map((column) => (
              <th
                key={String(column.key)}
                className={clsx(column.align === "end" && "zs-table__cell--end")}
              >
                {column.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={columns.length}>{empty}</td>
            </tr>
          ) : (
            rows.map((row, index) => (
              <tr key={getRowKey?.(row, index) ?? String(index)}>
                {columns.map((column) => (
                  <td
                    key={String(column.key)}
                    className={clsx(column.align === "end" && "zs-table__cell--end")}
                  >
                    {column.cell ? column.cell(row) : String(row[column.key] ?? "")}
                  </td>
                ))}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}

export interface EmptyStateProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  icon?: ReactNode;
}

export function EmptyState({
  title,
  description,
  action,
  icon,
  className,
  ...props
}: EmptyStateProps) {
  return (
    <div className={clsx("zs-empty", className)} {...props}>
      {icon && <div className="zs-empty__icon">{icon}</div>}
      <h2 className="zs-empty__title">{title}</h2>
      {description && <p className="zs-empty__description">{description}</p>}
      {action && <div className="zs-empty__action">{action}</div>}
    </div>
  );
}
