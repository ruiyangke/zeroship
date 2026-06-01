// ─── CanvasPills — crystal canvas tab switcher ──────────────────────
//
// Tier-gated switcher across the workspace canvases (preview / files /
// logs / env / settings). Maker tier shows preview only; ops adds
// logs/env/settings; code unlocks files too.
//
// Built over @zeroship/ui Toggle.Group — a single-selection segmented
// control. Each segment is a DS Toggle keyed by canvas id. The group is
// driven controlled via `value={active}`; selection is committed by the
// per-segment `onClick` (always fires `onChange(id)`, matching the
// previous hand-rolled Pill contract — re-clicking the active segment
// re-fires `onChange`, never a deselect). Bespoke sizing for the compact
// switcher lives in CanvasPills.css against --zs-* tokens.
//
// Tier visibility is OWNED BY THE PARENT: only the visible canvases are
// rendered, so e2e `toBeHidden()` / `toHaveCount(0)` assertions on the
// `pill:<id>` testids hold. The `canvas-pills` + `pill:<id>` testids and
// the public props/exports are preserved exactly.

import { Toggle } from "@zeroship/ui";
import "./CanvasPills.css";

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
    // Controlled single-selection segmented control. `value={active}`
    // reflects the pressed segment; selection is committed by each
    // segment's `onClick` (always `onChange(id)`), so the group's own
    // `onValueChange` is intentionally omitted — re-clicking the active
    // segment must re-fire `onChange`, not deselect.
    <Toggle.Group
      size="sm"
      value={active}
      equalWidth={false}
      aria-label="Canvas"
      data-testid="canvas-pills"
      className="canvas-pills"
    >
      {pills.map((id) => (
        <Toggle
          key={id}
          value={id}
          size="sm"
          onClick={() => onChange(id)}
          data-testid={`pill:${id}`}
        >
          {id}
        </Toggle>
      ))}
    </Toggle.Group>
  );
}
