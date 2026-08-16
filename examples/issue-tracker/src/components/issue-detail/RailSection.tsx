/**
 * A labelled divider between groups of rail fields.
 *
 * The rail was one uninterrupted column of thirteen controls, so status,
 * classification, people and location all had the same standing and the eye
 * had nowhere to rest. These are the joints.
 */
export function RailSection({ title }: { title: string }) {
  return (
    <p className="rail-section col-span-full mt-4 mb-2 border-t border-line pt-4 text-xs font-semibold tracking-[0.06em] text-ink-muted uppercase">
      {title}
    </p>
  );
}
