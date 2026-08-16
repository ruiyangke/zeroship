# Retire @zeroship/ui

Date: 2026-08-16

## Status

Accepted. Supersedes
[`2026-05-26-design-system.md`](./2026-05-26-design-system.md).

## Context

The headless UI package emitted `data-slot` hooks so a separate theme package
could style component internals that application call sites did not render.
Tailwind assigns that same visual-ownership role to classes at the render site.
The approaches solve the same problem incompatibly: one separates markup from
external slot styles, while the other requires the application to own both.

Base UI 1.5.0 already supplies accessible component parts, state data
attributes, and `render` composition directly. The issue tracker demonstrates
the replacement: local components under `src/ui/` compose Base UI and apply
Tailwind within the application.

## Decision

zeroship will not publish a UI SDK or theme package. Delete `sdks/ui`,
`sdks/ui-theme`, and `examples/apple-website-study`, including the class-to-slot
map, Storybook surface, publish entry, dependency state, and living-document
references.

Applications choose their UI dependencies and own their component wrappers,
tokens, and visual styles locally.

## Consequences

There is no platform UI package API, external slot stylesheet, or Storybook MCP
surface to maintain. The class-to-slot map disappears with the slots it mapped.
Existing design-system ADRs remain historical records, but the 2026-05-26
design-system decision is no longer current.
