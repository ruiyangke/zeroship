// ─── ChatTab — default tab ──────────────────────────────────────
// The chat rail is already mounted in the workspace shell, so this
// tab is just the live-preview iframe. Reload key comes from
// workspace context so the parent's deploy-success bumps surface
// here instantly.

import { useRef, useEffect } from "react";
import { Preview, type PreviewHandle } from "../../builder/components/Preview";
import { useWorkspace } from "../ProjectWorkspace";

export function ChatTab() {
  const { app, deployVersion, bumpDeployVersion } = useWorkspace();
  const previewRef = useRef<PreviewHandle>(null);

  // When the workspace bumps deployVersion (after agent deploy or
  // manual save), reload the iframe.
  useEffect(() => {
    previewRef.current?.reload();
  }, [deployVersion]);

  return (
    <div className="h-full flex flex-col" data-testid="chat-tab">
      <Preview
        ref={previewRef}
        appName={app?.name ?? null}
        deployVersion={deployVersion}
      />
      {/* Hidden hook so child mutations (rare) can request a reload. */}
      <button
        type="button"
        className="hidden"
        onClick={bumpDeployVersion}
        aria-hidden="true"
      />
    </div>
  );
}
