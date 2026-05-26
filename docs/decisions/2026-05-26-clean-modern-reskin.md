# Clean modern UI reskin

Date: 2026-05-26

## Context

`@zeroship/ui` still had the original Atelier-first look: serif display type by default, heavy tomato accents, sharp corners, uppercase labels, and dense shadows. For the pilot, the default design system needs to read as a clean modern product UI while preserving the existing token contract, ThemeProvider, three-theme architecture, and Base UI component primitives.

## Base UI references studied

- Base UI official component docs: Select, Menu, Dialog, Popover, Checkbox, Switch, Tabs, and Input.
- Base UI handbooks: Styling and Animation.
- Base UI GitHub demo CSS modules under `docs/src/app/(docs)/react/components/*/demos`.

## Values adopted

Base UI's styled examples consistently use compact, sentence-case controls with plain labels such as `Apple`, `Notifications`, `Overview`, and `Projects`. Labels are regular to bold at `0.875rem` with `1.25rem` line-height, not uppercase label treatments. Zeroship now uses sentence-case labels at `--zs-text-sm`, medium weight, and `--zs-ink-soft`.

The Base UI examples use control heights around `2rem`, small label-to-control gaps around `0.25rem`, item padding around `0.375rem` to `0.5rem`, and visible but restrained focus treatment: a `2px` outline with a slight negative or positive offset in their CSS modules. Zeroship adapted that into a tokenized `--zs-shadow-focus` soft ring and keeps Input, Select, Textarea, Checkbox, Switch, and Radio spacing aligned through one `FieldFrame` gap.

The Base UI popup examples set `transform-origin: var(--transform-origin)`, then transition `transform` and `opacity` with `data-starting-style` and `data-ending-style`. Their component demos use roughly `100ms` and `scale(0.98)`, while the animation handbook recommends CSS transitions and shows `150ms` scale/opacity motion. Zeroship adopted tokenized `--zs-motion-base` timing, transform-origin fallbacks, and `scale(0.98)` enter/exit states across Select, Menu, Popover, Tooltip, Dialog, and Toast.

The demo CSS uses crisp borders and simple shadows such as a low-alpha offset shadow on popups. Zeroship keeps the Base UI clarity but modernizes it with softer radii and layered low-alpha shadows: controls use `--zs-radius-md`, popups/cards use `--zs-radius-lg` to `--zs-radius-xl`, and shadows stay subtle through the existing `--zs-shadow-*` tokens.

The Base UI Switch demo uses explicit track, padding, thumb size, and checked translate values so the thumb actually travels from one edge to the other. Zeroship now computes checked thumb travel from the tokenized track width minus track height, fixing the previous stuck-left appearance.

## Decision

Make `studio` the default theme on `:root` and in `ThemeProvider`, with a neutral palette, sans display/body stack, restrained blue accent, soft radii, and layered low-alpha shadows. Keep `atelier` and `dusk` as switchable variants, but apply the same softer radius/shadow scale and sentence-case label treatment so the themes differ by palette and personality rather than by outdated execution.

The token contract remains unchanged. Components continue to reference `--zs-*` tokens only, and the legacy `--color-*` aliases remain mapped from the semantic tokens for existing builder CSS.
