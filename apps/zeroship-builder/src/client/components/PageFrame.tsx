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
          <div className="mx-auto px-6 py-12" style={{ maxWidth }}>
            {children}
          </div>
        ) : (
          <div
            className={cn("mx-auto px-12 py-12 grid gap-12", showMarginalia && "lg:gap-12")}
            style={{
              gridTemplateColumns: showMarginalia ? "200px 1fr" : "1fr",
              maxWidth: showMarginalia ? maxWidth + 220 : maxWidth,
            }}
          >
            {showMarginalia && <Marginalia />}
            <div>{children}</div>
          </div>
        )}
      </main>
    </div>
  );
}
