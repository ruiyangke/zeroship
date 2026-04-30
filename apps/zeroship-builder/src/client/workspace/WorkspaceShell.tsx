import { useState } from "react";
import { TopBar } from "./TopBar";
import { CanvasPills, type CanvasPillId } from "./CanvasPills";
import { PreviewCanvasStub } from "./PreviewCanvasStub";
import { ChatRail } from "./chat/ChatRail";

export interface WorkspaceShellProps {
  /** Plan 03 introduces real project routing; Plan 01 hardcodes "untitled". */
  projectName?: string;
}

export function WorkspaceShell({ projectName = "untitled" }: WorkspaceShellProps) {
  const [active, setActive] = useState<CanvasPillId>("preview");

  return (
    <div className="h-screen flex flex-col bg-paper">
      <TopBar
        projectName={projectName}
        center={
          <div className="flex items-center gap-3">
            <CanvasPills active={active} onChange={setActive} />
          </div>
        }
        right={
          <a
            href="#"
            data-testid="topbar-url"
            className="inline-flex items-center gap-2 px-3 py-1.5 border border-rule rounded-full bg-paper-2 font-mono text-[11px] text-ink-soft hover:border-ink hover:text-ink"
            style={{ textDecoration: "none" }}
          >
            <span className="size-[5px] rounded-full bg-ivy pulse-dot" aria-hidden="true" />
            {projectName}.zeroship.app
          </a>
        }
        accountInitials="ZS"
      />

      <div
        className="flex-1 grid min-h-0"
        style={{ gridTemplateColumns: "1fr 320px" }}
      >
        <main data-testid="canvas-area" className="min-h-0 min-w-0 overflow-hidden flex flex-col">
          {/* Plan 01 only renders preview; pill switching is wired but other
              canvases are placeholders. Plans 03–06 fill them in. */}
          {active === "preview" && <PreviewCanvasStub />}
          {active !== "preview" && (
            <div className="h-full flex items-center justify-center text-ink-soft font-serif italic">
              "{active}" canvas — coming in a later plan.
            </div>
          )}
        </main>
        <aside className="border-l border-rule min-h-0 min-w-0 overflow-hidden flex flex-col">
          <ChatRail appName={projectName} />
        </aside>
      </div>
    </div>
  );
}
