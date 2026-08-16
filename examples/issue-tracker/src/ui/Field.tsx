import { Field as BaseField } from "@base-ui/react/field";
import { forwardRef } from "react";

const ROOT_CLASSES = "flex min-w-0 flex-col gap-2 font-sans text-ink";
const LABEL_CLASSES =
  "inline-flex items-baseline gap-1 text-sm font-medium leading-snug text-ink-secondary";
const DESCRIPTION_CLASSES = "m-0 text-sm leading-snug text-ink-muted";

function resolveClassName<State>(
  baseClasses: string,
  className: string | ((state: State) => string | undefined) | undefined,
  state: State,
) {
  const consumerClasses =
    typeof className === "function" ? className(state) : className;
  return `${baseClasses}${consumerClasses ? ` ${consumerClasses}` : ""}`;
}

const Root = forwardRef<HTMLDivElement, BaseField.Root.Props>(function FieldRoot(
  { className, ...props },
  ref,
) {
  return (
    <BaseField.Root
      {...props}
      ref={ref}
      className={(state) => resolveClassName(ROOT_CLASSES, className, state)}
    />
  );
});

const Label = forwardRef<HTMLElement, BaseField.Label.Props>(function FieldLabel(
  { className, ...props },
  ref,
) {
  return (
    <BaseField.Label
      {...props}
      ref={ref}
      className={(state) => resolveClassName(LABEL_CLASSES, className, state)}
    />
  );
});

const Description = forwardRef<HTMLParagraphElement, BaseField.Description.Props>(
  function FieldDescription({ className, ...props }, ref) {
    return (
      <BaseField.Description
        {...props}
        ref={ref}
        className={(state) =>
          resolveClassName(DESCRIPTION_CLASSES, className, state)
        }
      />
    );
  },
);

Root.displayName = "Field.Root";
Label.displayName = "Field.Label";
Description.displayName = "Field.Description";

export const Field = { Root, Label, Description };
