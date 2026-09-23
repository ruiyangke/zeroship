import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import {
useEffect,
useState,
useId,
cloneElement,
type ReactNode,
type ReactElement,
} from "react";
import { Link } from "react-router-dom";
import {
ArrowRight,Leaf
} from "lucide-react";
import { Button,buttonVariants } from "@gather/meal-kit/components/ui/button";
import { Badge as ShadcnBadge } from "@gather/meal-kit/components/ui/badge";
import {
Field as FieldRoot,
FieldLabel,
FieldDescription,
FieldError,
} from "@gather/meal-kit/components/ui/field";
import { Alert,AlertTitle,AlertDescription } from "@gather/meal-kit/components/ui/alert";
import {
Empty as EmptyRoot,
EmptyHeader,
EmptyMedia,
EmptyTitle,
EmptyDescription,
EmptyContent,
} from "@gather/meal-kit/components/ui/empty";
import { Spinner } from "@gather/meal-kit/components/ui/spinner";
import { Input } from "@gather/meal-kit/components/ui/input";
import {
Dialog,
DialogContent,
DialogTitle,
DialogDescription,
} from "@gather/meal-kit/components/ui/dialog";
import { cn } from "@gather/meal-kit/lib/utils";
export { Select } from "@gather/meal-kit/components/select-field";

export { Button, Input, Dialog, DialogContent, DialogTitle, DialogDescription };
export function Field({
  label,
  children,
  hint,
  error,
}: {
  label: string;
  children: ReactElement<{
    id?: string;
    "aria-describedby"?: string;
    "aria-invalid"?: boolean;
  }>;
  hint?: string;
  error?: string;
}) {
  const generatedId = useId();
  const id = children.props.id ?? generatedId;
  const describedBy =
    [
      children.props["aria-describedby"],
      hint ? `${id}-hint` : undefined,
      error ? `${id}-error` : undefined,
    ]
      .filter(Boolean)
      .join(" ") || undefined;
  return (
    <FieldRoot className="field" data-invalid={!!error}>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      {cloneElement(children, {
        id,
        "aria-describedby": describedBy,
        "aria-invalid": !!error || children.props["aria-invalid"],
      })}
      {hint && <FieldDescription id={`${id}-hint`}>{hint}</FieldDescription>}
      {error && <FieldError id={`${id}-error`}>{error}</FieldError>}
    </FieldRoot>
  );
}
export function Badge({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <ShadcnBadge variant="secondary" className={cn("badge", className)}>
      {children}
    </ShadcnBadge>
  );
}
export function SectionTitle({
  eyebrow,
  title,
  body,
  children,
}: {
  eyebrow?: string;
  title: string;
  body?: string;
  children?: ReactNode;
}) {
  return (
    <div className="section-title">
      <div>
        {eyebrow && <p className="eyebrow">{eyebrow}</p>}
        <h1>{title}</h1>
        {body && <p className="section-description">{body}</p>}
      </div>
      {children}
    </div>
  );
}
export function Empty({
  title,
  text,
  children,
}: {
  title: string;
  text: string;
  children?: ReactNode;
}) {
  return (
    <EmptyRoot className="empty-state">
      <EmptyHeader>
        <EmptyMedia>
          <Leaf size={36} />
        </EmptyMedia>
        <EmptyTitle>
          <h2>{title}</h2>
        </EmptyTitle>
        <EmptyDescription>{text}</EmptyDescription>
      </EmptyHeader>
      {children && <EmptyContent>{children}</EmptyContent>}
    </EmptyRoot>
  );
}
export function Loading() {
  const { _: t } = useLingui();
  return (
    <div role="status" className="loading">
      <Spinner aria-hidden="true" role="presentation" />
      <span>{t(msg`Loading…`)}</span>
    </div>
  );
}
export function ErrorState({
  error,
  retry,
}: {
  error: string;
  retry: () => void;
}) {
  const { _: t, i18n } = useLingui();
  return (
    <Alert variant="destructive" className="error-panel">
      <AlertTitle>
        <h2>{t(msg`Let's try that again`)}</h2>
      </AlertTitle>
      <AlertDescription>
        {Object.hasOwn(i18n.messages, error) ? t(error) : error}
      </AlertDescription>
      <Button variant="outline" onClick={retry}>
        {t(msg`Retry`)}
      </Button>
    </Alert>
  );
}
export function useLoad<T>(load: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T>();
  const [error, setError] = useState("");
  const [reload, setReload] = useState(0);
  useEffect(() => {
    let live = true;
    setError("");
    setData(undefined);
    load()
      .then((v) => {
        if (live) setData(v);
      })
      .catch((e) => {
        if (live) setError(String(e.message ?? e));
      });
    return () => {
      live = false;
    };
  }, [...deps, reload]);
  return { data, error, refresh: () => setReload((n) => n + 1) };
}
export function CtaLink({
  to,
  children,
  outline = false,
  className,
}: {
  to: string;
  children: ReactNode;
  outline?: boolean;
  className?: string;
}) {
  return (
    <Link
      className={cn(
        buttonVariants({
          variant: outline ? "outline" : "default",
          size: "lg",
        }),
        "cta",
        outline && "cta-outline",
        className,
      )}
      to={to}
    >
      {children}
      <ArrowRight size={17} />
    </Link>
  );
}
