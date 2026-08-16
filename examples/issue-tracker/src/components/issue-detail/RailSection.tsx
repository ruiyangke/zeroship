/**
 * A labelled divider between groups of rail fields.
 *
 * The rail was one uninterrupted column of thirteen controls, so status,
 * classification, people and location all had the same standing and the eye
 * had nowhere to rest. These are the joints.
 */
export function RailSection({ title }: { title: string }) {
  return <p className="rail-section">{title}</p>;
}
