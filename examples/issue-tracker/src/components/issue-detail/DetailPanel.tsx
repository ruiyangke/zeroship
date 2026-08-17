import type { ReactNode } from "react";

type DetailPanelLocator = "attachments-panel" | "flags-panel" | "security-panel" | "votes-panel";

/**
 * One of the small panels below the issue conversation.
 *
 * The locator is deliberately separate from styling: Playwright uses it to
 * address the same section, while this component owns the shared title and
 * section layout.
 */
export function DetailPanel({
  locator,
  title,
  children,
}: {
  locator: DetailPanelLocator;
  title: string;
  children: ReactNode;
}) {
  return (
    <section className={`${locator} m-0 min-w-0`}>
      <h3 className="mb-2 text-xs font-semibold tracking-wide text-ink-muted uppercase">
        {title}
      </h3>
      {children}
    </section>
  );
}
