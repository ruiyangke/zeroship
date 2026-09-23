import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { cn } from "cn";
import { Loader2Icon } from "lucide-react";

function Spinner({ className, ...props }: React.ComponentProps<"svg">) {
  const { _: t } = useLingui();
  return (
    <Loader2Icon
      data-slot="spinner"
      role="status"
      aria-label={t(msg`Loading…`)}
      className={cn("size-4 animate-spin", className)}
      {...props}
    />
  );
}

export { Spinner };
