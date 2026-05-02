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

/**
 * Three tiers per spec §1.5 / §3.2. Each progressively reveals more
 * canvases. The tier is set at the workspace level and persisted in
 * localStorage. Plain progressive disclosure — Maker is the default,
 * +Data adds the database-shaped surfaces, +Code unlocks everything
 * (including Files for those who want to read the source).
 */
export type CanvasTier = "maker" | "data" | "code";

const MAKER_PILLS: readonly CanvasPillId[] = [
  "preview",
  "logs",
  "env",
  "plan",
  "health",
  "settings",
] as const;

const PLUS_DATA_PILLS: readonly CanvasPillId[] = [
  "preview",
  "data",
  "media",
  "logs",
  "env",
  "plan",
  "health",
  "settings",
] as const;

const PLUS_CODE_PILLS: readonly CanvasPillId[] = ALL_PILLS;

/**
 * Resolve the visible pill list for a tier. Exported so non-pill
 * callers (e.g. WorkspaceShell deciding whether the active pill is
 * still visible after a tier change) share one source of truth.
 */
export function pillsForTier(tier: CanvasTier): readonly CanvasPillId[] {
  if (tier === "code") return PLUS_CODE_PILLS;
  if (tier === "data") return PLUS_DATA_PILLS;
  return MAKER_PILLS;
}

export interface CanvasPillsProps {
  active: CanvasPillId;
  onChange: (id: CanvasPillId) => void;
  /**
   * Explicit override — when a parent wants to drive visibility itself.
   * Most callers should pass `tier` instead.
   */
  visible?: readonly CanvasPillId[];
  /**
   * Tier — drives the pill list when `visible` isn't supplied. Default
   * "code" preserves the pre-tier behaviour (every pill always shown)
   * for callers that haven't opted in to tier filtering yet.
   */
  tier?: CanvasTier;
}

export function CanvasPills({ active, onChange, visible, tier = "code" }: CanvasPillsProps) {
  const pills = visible ?? pillsForTier(tier);
  return (
    <div data-testid="canvas-pills" className="flex items-center gap-1.5">
      {pills.map((id) => (
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
