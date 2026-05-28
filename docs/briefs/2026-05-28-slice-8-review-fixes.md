# Slice 8 review fixes — Form + Fieldset

**Worktree** `.worktrees/wave1-slice8` on branch `wave1-slice8`.

Codex 3🔴+3🟡 + Claude 0🔴+0🟡+0🟢 + 4 sub-threshold FYIs. The shared 🔴 (aria-wiring syntax) was fixed inline on builder/ui-design. Remaining: 2🔴 + 3🟡 = 5 items.

## Hard constraints

Standard set. DO NOT commit; orchestrator merges via wave1-slice8 branch.

## Fix list

### 🔴 (2)

**1. Stories put `name` on `Input`, not `Field`.**
`Form.stories.tsx:232`, `Fieldset.stories.tsx:166`.

Base UI submits / routes errors by `Field.Root` name. `<Input name="email">` alone silently drops values + errors. Canonical: `<Field name="email"><Input … /></Field>`. Fix all Form + Fieldset stories that use the name attribute.

**2. Fieldset disabled cascade promise vs Input-only assertion.**
`Fieldset.tsx:24`, brief promised cascade to ALL nested controls.

Custom controls (Checkbox/Switch/Radio/Toggle/etc.) have non-native visible roots and don't pick up native `<fieldset disabled>` cascade. Fix: add a `FieldsetDisabledContext` that the selection-row helpers consume; Slice 4/5 primitives already consume `useFieldDisabledContext()`. Either:
- Plumb Fieldset's disabled into the existing Field disabled context, OR
- Add a separate Fieldset context and update Checkbox/Switch/Radio/Toggle to read both.

Decision (autonomous): add new `FieldsetDisabledContext` to keep concerns separate; selection primitives check both. Document inline.

### 🟡 (3)

**3. `FormValues extends Record<string, unknown>` rejects interface-shaped form values.**
`Form.tsx:68`.

Fix: `FormValues extends object = Record<string, unknown>`. Keep internal Base UI casts as-is.

**4. `FormActions` not exported.**
`Form.tsx:97` + `Form.stories.tsx:3` imports Base UI's type.

Fix: `export type FormActions = BaseForm.Actions;` from Form/index.ts and re-export from components/index.ts + src/index.ts.

**5. `Fieldset.Legend` exposes Base UI `render` while ref is fixed to HTMLDivElement.**
`Fieldset.tsx:92`.

Fix: omit `render` from `LegendProps` (`Omit<BaseFieldsetLegendProps, "render">`). Or add proper polymorphic API — not in scope; do the omit.

## Files to modify

- `sdks/ui/src/components/Form/Form.tsx` — items 3, 4.
- `sdks/ui/src/components/Form/index.ts` — item 4.
- `sdks/ui/src/components/Fieldset/Fieldset.tsx` — items 2, 5.
- `sdks/ui/src/stories/Form.stories.tsx` — item 1.
- `sdks/ui/src/stories/Fieldset.stories.tsx` — item 1.
- `sdks/ui/src/components/Field/Field.tsx` — item 2 (export new context hook or extend existing).
- `sdks/ui/src/components/Checkbox/Checkbox.tsx` — item 2 (consume new context).
- `sdks/ui/src/components/Switch/Switch.tsx` — item 2.
- `sdks/ui/src/components/Radio/Radio.tsx` — item 2.
- `sdks/ui/src/components/Toggle/Toggle.tsx` — item 2.

## Verification gates

1. `pnpm --filter @zeroship/ui build` — green.
2. `pnpm exec tsc --noEmit` — clean.
3. `pnpm --filter @zeroship/ui build-storybook` — green.
4. `pnpm --filter zeroship-builder build` — green.
5. Token purity x5 + raw `oklch(` = 0.
6. `node --check sdks/ui/scripts/check-aria-wiring.mjs` — clean.
7. A11y clean.
8. Aria-wiring all PASS.

## Contingencies

- Item 2: if Fieldset disabled context is added, the existing Fieldset disabled aria-wiring assertion already passes via native cascade — extend to assert disabled propagates to a nested Checkbox/Switch/Radio inside the Fieldset.

**WORKTREE**: `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice8`. Leave uncommitted.
