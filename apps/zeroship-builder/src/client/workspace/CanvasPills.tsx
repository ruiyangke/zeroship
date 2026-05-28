import { Pill } from "../components/Pill";

const ALL_PILLS = [
  "preview",
  "files",
  "logs",
  "env",
  "settings",
] as const;

export type CanvasPillId = (typeof ALL_PILLS)[number];

/**
 * Three tiers per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §1.5 / §3.2.
 * Maker is intentionally small: preview + chat is the creator-facing
 * product. The other tiers expose advanced evidence surfaces only when
 * the user asks for them.
 */
export type CanvasTier = "maker" | "ops" | "code";

const MAKER_PILLS: readonly CanvasPillId[] = [
  "preview",
] as const;

const PLUS_OPS_PILLS: readonly CanvasPillId[] = [
  "preview",
  "logs",
  "env",
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
  if (tier === "ops") return PLUS_OPS_PILLS;
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
   * Tier — drives the pill list when `visible` isn't supplied.
   */
  tier?: CanvasTier;
}

export function CanvasPills({ active, onChange, visible, tier = "maker" }: CanvasPillsProps) {
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
