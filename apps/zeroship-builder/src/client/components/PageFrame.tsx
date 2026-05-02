// ─── PageFrame — common shell for creator content pages ─────────
//
// TopBar + center column with optional left marginalia. Pages just
// drop their content as children and get the editorial frame for free.

import type { ReactNode } from "react";
import { TopBar, type TopBarProps } from "./TopBar";
import { Marginalia } from "./Marginalia";
import { cn } from "../lib/utils";

export interface PageFrameProps extends TopBarProps {
  children: ReactNode;
  /** When true, renders the Vol./issue marginalia rail. Defaults true. */
  showMarginalia?: boolean;
  /** When true, the content column is centered with no left rail (e.g. for auth). */
  centered?: boolean;
  /** Width of content column. Default: 760px. */
  maxWidth?: number;
}

export function PageFrame({
  children,
  showMarginalia = true,
  centered = false,
  maxWidth = 760,
  ...topbar
}: PageFrameProps) {
  return (
    <div className="min-h-screen flex flex-col">
      <TopBar {...topbar} />
      <main className="flex-1 overflow-auto">
        {centered ? (
          <div className="mx-auto px-4 sm:px-6 py-8 sm:py-12" style={{ maxWidth }}>
            {children}
          </div>
        ) : (
          // Marginalia stacks above the content column on phones, then
          // moves to a left rail at lg+ where there's room for it.
          // px-4 on phone keeps the eyeline tight; px-12 lands on
          // tablet/desktop. Grid template falls back to single-column
          // below the lg breakpoint via the `lg:` modifier.
          <div
            className={cn(
              "mx-auto px-4 sm:px-6 lg:px-12 py-8 sm:py-12 grid gap-8 lg:gap-12",
              showMarginalia ? "grid-cols-1 lg:grid-cols-[200px_1fr]" : "grid-cols-1",
            )}
            style={{
              maxWidth: showMarginalia ? maxWidth + 220 : maxWidth,
            }}
          >
            {showMarginalia && <Marginalia />}
            <div className="min-w-0">{children}</div>
          </div>
        )}
      </main>
    </div>
  );
}
