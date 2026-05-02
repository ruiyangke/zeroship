import { Pill } from "../components/Pill";

const ALL_PILLS = [
  "preview",
  "files",
  "data",
  "media",
  "logs",
  "env",
  "plan",
  "health",
  "settings",
] as const;

export type CanvasPillId = (typeof ALL_PILLS)[number];

export interface CanvasPillsProps {
  active: CanvasPillId;
  onChange: (id: CanvasPillId) => void;
  /** Visible pill set (filtered by tier — Plan 02+ wires this). */
  visible?: readonly CanvasPillId[];
}

export function CanvasPills({ active, onChange, visible = ALL_PILLS }: CanvasPillsProps) {
  return (
    <div data-testid="canvas-pills" className="flex items-center gap-1.5">
      {visible.map((id) => (
        <Pill
          key={id}
          size="sm"
          active={active === id}
          onClick={() => onChange(id)}
          data-testid={`pill:${id}`}
        >
          {id}
        </Pill>
      ))}
    </div>
  );
}
