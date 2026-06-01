// ─── PageFrame — common shell for creator content pages ─────────
//
// TopBar + a centered content column. Pages drop their content as
// children and get the authed crystal frame for free.
//
// Built over @zeroship/ui: a full-height column with the authed TopBar
// pinned at the top and a scrolling <main> holding a centered DS
// Container. The editorial Marginalia rail is retired; `showMarginalia`
// is kept as a no-op so existing callers still typecheck.

import type { ReactNode } from "react";
import { Container } from "@zeroship/ui";
import { TopBar, type TopBarProps } from "./TopBar";
import "./PageFrame.css";

export interface PageFrameProps extends TopBarProps {
  children: ReactNode;
  /**
   * Retired no-op. The marginalia rail (an editorial concept) is gone;
   * the prop is kept so callers passing `showMarginalia` still typecheck.
   */
  showMarginalia?: boolean;
  /** When true, the content column is centered with no left rail (e.g. for auth). */
  centered?: boolean;
  /** Width of content column. Default: 760px. */
  maxWidth?: number;
}

export function PageFrame({
  children,
  // Retired — the marginalia rail no longer exists. Destructured here so
  // it doesn't fall through to TopBar via `...topbar`.
  showMarginalia: _showMarginalia,
  // Retained for signature compatibility; the layout is always a single
  // centered column now that the marginalia rail is gone.
  centered: _centered,
  maxWidth = 760,
  ...topbar
}: PageFrameProps) {
  return (
    <div className="page-frame">
      <TopBar {...topbar} />
      <main className="page-frame__main">
        <Container
          size="full"
          padX={4}
          className="page-frame__column"
          style={
            // Preserve the numeric px `maxWidth` prop exactly by feeding
            // it to the Container's width custom property (px → rem).
            { "--container-max-width": `${maxWidth / 16}rem` } as React.CSSProperties
          }
        >
          {children}
        </Container>
      </main>
    </div>
  );
}
