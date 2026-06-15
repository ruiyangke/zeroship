import { ChevronRight } from "lucide-react";
import { Button } from "@zeroship/ui";

export function SectionHeader({
  title,
  id,
  actionLabel,
  actionHref = "#lineup",
  solo = false,
}: {
  title: string;
  id: string;
  actionLabel?: string;
  actionHref?: string;
  solo?: boolean;
}) {
  return (
    <header className={`section-header${solo ? " section-header--solo" : ""}`}>
      <h2 id={id}>{title}</h2>
      {actionLabel ? (
        <Button asChild variant="plain">
          <a href={actionHref}>
            {actionLabel}
            <ChevronRight aria-hidden="true" size={17} strokeWidth={2} />
          </a>
        </Button>
      ) : null}
    </header>
  );
}
