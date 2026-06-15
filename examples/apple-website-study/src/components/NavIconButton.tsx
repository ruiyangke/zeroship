import type { LucideIcon } from "lucide-react";
import { Button, Icon } from "@zeroship/ui";

export function NavIconButton({ label, icon }: { label: string; icon: LucideIcon }) {
  return (
    <Button className="apple-demo-nav-icon" variant="plain" size="small" aria-label={label}>
      <Icon as={icon} size="sm" />
    </Button>
  );
}
