import { EmptyState } from "../components/EmptyState";

/** Plan 01 placeholder — real preview canvas in Plan 04. */
export function PreviewCanvasStub() {
  return (
    <div data-testid="preview-canvas" className="h-full flex items-center justify-center bg-paper-2">
      <EmptyState
        title="Nothing's been built yet."
        description="Tell the agent what to make in the chat on the right."
      />
    </div>
  );
}
