# Slice 8 — Form + Fieldset (structural)

**Worktree** `.worktrees/wave1-slice8` on branch `wave1-slice8` off `builder/ui-design@e17f6655`.

Two structural wrappers from Base UI: `form` (consolidated submit + validation surface) and `fieldset` (labeled group of fields with a Legend).

## Goal

- **Form** — wraps `<form>`, coordinates Field.Root validation, exposes `onFormSubmit(formValues, details)` callback and `validate()` imperative ref.
- **Fieldset** — wraps `<fieldset>` + `<legend>`. Cascades `disabled` to all nested controls.

## Hard constraints

- Pre-launch, no back-compat.
- `--zs-*` tokens only. No raw hex / px / oklch() in component CSS.
- HIG as principle but NOT in source.
- `prefers-reduced-motion`, `@media (forced-colors: active)` with state-selector specificity mirror (Slice 5/6/7 lesson), RTL via logical properties.
- Real-path aria-wiring.
- DO NOT commit your own commit — leave changes staged. Orchestrator merges to builder/ui-design.

## API shape

```tsx
// Form
export interface FormProps<FormValues extends Record<string, any> = Record<string, any>>
  extends Omit<BaseFormProps<FormValues>, "render"> {
  className?: string;
  /** Visual variant — `default` no surface, `card` opaque surface with rim. */
  variant?: "default" | "card";
}

// Fieldset namespace
export interface FieldsetProps extends Omit<BaseFieldsetRootProps, "render"> {
  /** Inline padding cadence — sm/md/lg matches Field rhythm. */
  size?: "sm" | "md" | "lg";
  className?: string;
}

export const Fieldset = ForwardedFieldset as FieldsetComponent & {
  Legend: typeof FieldsetLegend;
};
```

## Files to create

```
sdks/ui/src/components/Form/Form.{tsx,css}, index.ts (~120 lines)
sdks/ui/src/components/Fieldset/Fieldset.{tsx,css}, index.ts (~180 lines, namespace with Legend)
sdks/ui/src/stories/Form.stories.tsx (8 stories)
sdks/ui/src/stories/Fieldset.stories.tsx (8 stories)
sdks/ui/scripts/capture-form-evidence.mjs (NEW)
sdks/ui/scripts/capture-fieldset-evidence.mjs (NEW)
```

## Files to modify

- `sdks/ui/src/components/index.ts` — add Form + Fieldset exports.
- `sdks/ui/src/index.ts` — re-export prop types.
- `sdks/ui/src/styles.css` — `@import` for the 2 new CSS files.
- `sdks/ui/scripts/check-storybook-a11y.mjs` — register 16 new story IDs.
- `sdks/ui/scripts/check-aria-wiring.mjs` — add 4 new assertions.

## Story matrix (16 total)

### Form (8)
1. Basic submit handler · 2. WithValidation (server errors) · 3. ValidationModes (onSubmit/onBlur/onChange) · 4. Variants (default/card) · 5. WithFields (Field+Input combined) · 6. ActionsRef-validate · 7. Disabled (all controls disabled) · 8. RTL.

### Fieldset (8)
9. Basic with Legend · 10. AllSizes · 11. NestedFields (3 Fields inside) · 12. Disabled (cascade) · 13. WithFormIntegration · 14. NestedFieldset · 15. CustomLegendPosition · 16. RTL.

## Aria-wiring (4 new)

1. Form submit → onFormSubmit fires with collected formValues.
2. Form actionsRef.validate() programmatically invokes Field validation.
3. Fieldset disabled cascades aria-disabled to nested inputs.
4. Fieldset.Legend's id is referenced by aria-labelledby on the fieldset.

## Verification gates

1. `pnpm --filter @zeroship/ui build` → green.
2. `pnpm exec tsc --noEmit` → clean.
3. `pnpm --filter @zeroship/ui build-storybook` → green.
4. `pnpm --filter zeroship-builder build` → green.
5. Token purity x5 + raw `oklch(` = 0.
6. A11y clean for 197 stories (181 + 16 new).
7. Aria-wiring 69 PASS + 2 SKIP + 0 FAIL (65 + 4 new).
8. 16 PNGs captured.

## Contingencies (decide inline)

- **Form validation context**: Base UI's Form coordinates Field.Root validation via FormContext. Make sure Field children pick it up automatically (no extra wiring needed at consumer site).
- **Fieldset.Legend semantic**: HTML `<legend>` must be FIRST child of `<fieldset>`. Document inline.
- **Fieldset.Root disabled cascade**: Base UI handles this natively. Verify by checking nested Input/Checkbox renders with disabled visuals.
- **Card variant on Form**: use Card's surface tokens (--zs-card-surface, --zs-shadow-card) so Form variant=card reads visually consistent with Card.
- **Forced-colors mirror**: every state selector at equal specificity inside @media (forced-colors: active). Slice 5/6/7 lesson.

## Report

End with files changed; per-component API (file:line); token purity; tsc + build status; a11y count; aria-wiring counts; contingencies fired; 16 screenshot paths; one taste note; "I did NOT commit, push, or merge."

**WORKTREE NOTE**: Work in `/home/ruiyang/Projects/appbase/.worktrees/wave1-slice8`. Leave changes uncommitted; the orchestrator will merge to builder/ui-design from there.
