# Slice brief: FormSection block

A governed settings/form **section**: a header (title + description) + a body of
fields + an optional footer action row, in stacked or aside layout. The
"settings panel" pattern. Composes Field/Input/Stack/Cluster/Separator/Button.
Lives in `sdks/ui/src/blocks/FormSection/`. (Complements the `Fieldset`
PRIMITIVE — Fieldset = grouped controls w/ legend; FormSection = a higher-level
labelled section of a settings/form page.)

WORKTREE: `/home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks`.
> **DO NOT commit, push, or merge.** Implement + self-verify + report.

## Reference / compose
`components/Field`+`Input` (the fields consumers place in the body),
`components/Button` (footer actions), `layouts/{Stack,Cluster}`,
`components/Separator` (footer divider), `components/_slot`/_classnames. House
style; mirror Card's dual ergonomic+compound surface + heading discipline.

## API (dual surface)
```ts
export type FormSectionOrientation = "stacked" | "aside";
interface FormSectionProps extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  title?: ReactNode;            // section heading
  description?: ReactNode;      // muted supporting copy under the title
  orientation?: FormSectionOrientation; // "stacked" (default) | "aside" (header start col, body end col — settings style)
  footer?: ReactNode;           // action row (e.g. Save/Cancel Buttons)
  children?: ReactNode;         // the form body (Fields/Inputs) — or compound parts
}
// Compound: FormSection.Header (.Title/.Description) / .Body / .Footer
```
- Root `<section data-slot="form-section">` with an accessible name: the Title
  renders as a heading (`<h3>` default, `asChild` to relevel) and the section is
  `aria-labelledby` that heading's id (useId). Description = muted `<p>`.
- **stacked**: header (title+description) above, body (a vertical Stack of the
  consumer's fields) below, footer last (Separator + a right-aligned Cluster of
  actions). **aside**: header in a start column, body in the end column (settings
  page two-column; collapses to stacked below a breakpoint — reuse the
  `--zs-bp-*` tokens); footer spans under the body.
- Ergonomic `title`/`description`/`footer` props AND compound parts (two-mode,
  document precedence like the other blocks — no false suppression claim).
- `forwardRef`, per-prop JSDoc, data-slot vocabulary (form-section / -header /
  -title / -description / -body / -footer).

## a11y
`<section aria-labelledby={titleId}>`; Title is a relevelable heading; fields are
the consumer's Field/Input (labels/aria come from them); footer actions are real
Buttons. Don't add roles to body/footer (layout-only). forced-colors: the footer
Separator + header text stay legible.

## Constraints
`--zs-*` only; no raw hex/px; logical properties; forced-colors + reduced-motion;
no "HIG"/"Apple"; pre-launch no-back-compat. Export from `blocks/index.ts`;
`@import` CSS into styles.css (Composed blocks); story `layout: "fullscreen"`.

## Stories (`src/stories/FormSection.stories.tsx`)
- Stacked (title + description + a couple of Fields/Inputs + a Save/Cancel
  footer), Aside (settings two-column), Compound (the parts form), NoFooter.
- **play()**: assert the section is labelled by its heading (`getByRole("region"
  ... )`-ish or aria-labelledby resolves to the title text); a footer Button
  click fires its handler (fn spy). axe clean.

## Verify (run, REPORT; do not commit)
```bash
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks
pnpm --filter @zeroship/ui build && pnpm --filter @zeroship/ui build-storybook
cd sdks/ui/src && echo "hex/px: $(grep -rEn '#[0-9a-fA-F]{3,8}\b' --include='*.css' --include='*.tsx' blocks/FormSection|wc -l)/$(grep -rEn '[0-9]+px' --include='*.css' --include='*.tsx' blocks/FormSection|wc -l)"
cd /home/ruiyang/Projects/appbase/.claude/worktrees/ui-layouts-blocks/sdks/ui
(npx http-server storybook-static -p 6273 --silent &) ; sleep 3
npx test-storybook --config-dir .storybook --url http://127.0.0.1:6273 --maxWorkers=1 FormSection.stories 2>&1 | grep -E 'Tests:|✕'
```
(Dev server on :6006 — use 6273.) Report: files, stacked/aside layout method,
the section-labelledby a11y, build/grep/suite counts, decisions.
