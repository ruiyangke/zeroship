# @zeroship/ui story conventions

Stories are the single source of truth for what each component does.
The Test Runner visits every story (smoke render + axe a11y) and any
`play()` block runs as a real interaction test in Chromium. Coverage
is harvested by addon-coverage on the same pass.

## When to write a `play()`

Write one when behavior beats markup:

- **Interaction logic** — click, type, drag, keyboard navigation.
- **Controlled-state round-trips** — does the component report the
  right value back through `onChange`?
- **Focus order** — Tab/Shift+Tab through a composite (Menu,
  Toolbar, Form) lands in the expected sequence.
- **ARIA flips** — `aria-expanded`, `aria-checked`, `aria-pressed`
  toggling on user action.

## When NOT to

Pure rendering is already covered by the smoke pass. If the story is
"here is the Filled variant" and clicking it does nothing meaningful,
don't add a `play()`. The runner has already proven it renders and is
axe-clean.

## Pattern

```ts
import { expect, userEvent, within } from "@storybook/test";

export const ClickInteraction: Story = {
  render: () => <Button>Save</Button>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const button = canvas.getByRole("button", { name: /save/i });
    await userEvent.click(button);
    await expect(button).toHaveFocus();
  },
};
```

- Scope queries to `within(canvasElement)` — never `screen.*`.
- Query by accessible role + accessible name; avoid `data-testid`.
- Use `userEvent`, not raw `fireEvent` — userEvent dispatches the full
  pointer/keyboard sequence the way a real person would.
- Assert with `expect(...)` from `@storybook/test`; it's the same
  Jest-DOM matcher set the runner reports through.

## Skipping axe per story

```ts
parameters: { a11y: { disable: true } }      // skip entirely
parameters: { a11y: { config: { rules: [{ id: "color-contrast", enabled: false }] } } }
```

Use sparingly. A skipped a11y rule should come with a comment
explaining why the violation is intentional (e.g. demo of an
intentionally-invalid state).

## Standalone scripts → `play()` (later)

`scripts/check-aria-wiring.mjs` and `scripts/check-storybook-a11y.mjs`
predate the Test Runner. They run their own Chromium and re-assert
things the runner already covers. They stay in place for now; a
follow-up slice will fold each assertion into a `play()` on the
relevant story and delete the scripts.

## MCP for AI agents

When `pnpm storybook` is running, the MCP server at
`http://127.0.0.1:6006/mcp` exposes the same component manifest
agents need to write correct stories: `list-all-documentation` returns
every component id, `get-documentation` returns props + examples for
one. Call it before guessing a prop name.
