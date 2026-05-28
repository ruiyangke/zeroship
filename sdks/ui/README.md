# @zeroship/ui

Governed React design system for zeroship: semantic tokens, themes,
and composable primitives built on Base UI. Components are
documented in Storybook; Storybook is the public API reference.

## Development

```bash
pnpm --filter @zeroship/ui build              # tsup → dist/
pnpm --filter @zeroship/ui storybook          # dev at http://127.0.0.1:6006
pnpm --filter @zeroship/ui build-storybook    # static export at storybook-static/
```

## Testing

The Storybook Test Runner is the single quality gate. It visits every
story in real Chromium, runs any `play()` block as an interaction
test, and asserts the story is axe-clean. addon-coverage harvests
Istanbul coverage on the same pass.

```bash
pnpm --filter @zeroship/ui test-storybook            # against a running storybook on :6119
pnpm --filter @zeroship/ui test-storybook:ci         # boots http-server on :6119 + runs runner
pnpm --filter @zeroship/ui test-storybook:coverage   # + lcov/html report at sdks/ui/coverage/
```

Conventions for writing `play()` interactions and selecting elements
live in `.storybook/CONVENTIONS.md`.

## AI MCP integration

When `pnpm --filter @zeroship/ui storybook` is running, an MCP server
is mounted at `http://127.0.0.1:6006/mcp`. AI agents that integrate
with Model Context Protocol can call:

- `list-all-documentation` — every component ID
- `get-documentation` — props and examples for one component
- `get-story-documentation` — single-story detail

The component manifest is synthesized from `src/stories/` at dev-server
boot.
