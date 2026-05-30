import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { Tag } from "../blocks";

const meta: Meta<typeof Tag> = {
  title: "Blocks/Tag",
  component: Tag,
  parameters: { layout: "fullscreen" },
  argTypes: {
    size: { control: "inline-radio", options: ["sm", "md"] },
  },
};

export default meta;

type Story = StoryObj<typeof Tag>;

/* ─── 1. Default (static) ────────────────────────────────────────────── */
export const Default: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "A plain static tag. The root is a `<span>` — no interactive " +
          "descendants, no remove button, no toggle.",
      },
    },
  },
  render: () => <Tag data-testid="tag-default">React</Tag>,
};

/* ─── 2. With a leading icon ──────────────────────────────────────────── */
export const WithLeadingIcon: Story = {
  name: "With leadingIcon",
  parameters: {
    docs: {
      description: {
        story:
          "`leadingIcon` is decorative — wrapped in an `aria-hidden` span " +
          "so it never pollutes the accessible name. Here a small color " +
          "swatch precedes the label.",
      },
    },
  },
  render: () => (
    <Tag
      data-testid="tag-icon"
      leadingIcon={
        <svg viewBox="0 0 8 8" width="8" height="8" focusable="false">
          <circle cx="4" cy="4" r="4" fill="currentColor" />
        </svg>
      }
    >
      TypeScript
    </Tag>
  ),
};

/* ─── 3. Removable ───────────────────────────────────────────────────── */
export const Removable: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Removable mode: the root stays a `<span>` and a trailing real " +
          "`<button aria-label=\"Remove …\">` is the only interactive " +
          "descendant (no button-in-button). The remove label is derived " +
          "from the string children. play(): click the × → `onRemove` " +
          "fires; then focus the × and press Delete → `onRemove` fires " +
          "again.",
      },
    },
  },
  args: { onRemove: fn() },
  render: (args) => (
    <Tag data-testid="tag-removable" removable onRemove={args.onRemove}>
      React
    </Tag>
  ),
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    // The remove button derives its accessible name from the children.
    const remove = canvas.getByRole("button", { name: /remove react/i });

    // Click → onRemove.
    await userEvent.click(remove);
    await expect(args.onRemove).toHaveBeenCalledTimes(1);

    // Focus the × and press Delete → onRemove again.
    remove.focus();
    await expect(remove).toHaveFocus();
    await userEvent.keyboard("{Delete}");
    await expect(args.onRemove).toHaveBeenCalledTimes(2);

    // And Backspace also fires it.
    await userEvent.keyboard("{Backspace}");
    await expect(args.onRemove).toHaveBeenCalledTimes(3);
  },
};

/* ─── 4. Filter toggle ───────────────────────────────────────────────── */
export const Filter: Story = {
  parameters: {
    docs: {
      description: {
        story:
          "Filter mode: providing `selected` / `onSelectedChange` makes the " +
          "ROOT itself a `<button aria-pressed>` (no nested remove button). " +
          "Clicking flips `aria-pressed` and fires `onSelectedChange`. The " +
          "selected state paints with the accent fill.",
      },
    },
  },
  args: { onSelectedChange: fn() },
  render: (args) => {
    const ControlledFilter = () => {
      const [selected, setSelected] = useState(false);
      return (
        <Tag
          data-testid="tag-filter"
          selected={selected}
          onSelectedChange={(next) => {
            setSelected(next);
            args.onSelectedChange?.(next);
          }}
        >
          Open issues
        </Tag>
      );
    };
    return <ControlledFilter />;
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const chip = canvas.getByRole("button", { name: /open issues/i });

    // Starts unpressed.
    await expect(chip).toHaveAttribute("aria-pressed", "false");

    // Click → pressed flips + handler fires with `true`.
    await userEvent.click(chip);
    await expect(chip).toHaveAttribute("aria-pressed", "true");
    await expect(args.onSelectedChange).toHaveBeenLastCalledWith(true);

    // Click again → back to unpressed + handler fires with `false`.
    await userEvent.click(chip);
    await expect(chip).toHaveAttribute("aria-pressed", "false");
    await expect(args.onSelectedChange).toHaveBeenLastCalledWith(false);
  },
};

/* ─── 5. Sizes ───────────────────────────────────────────────────────── */
export const Sizes: Story = {
  name: "Sizes (sm / md)",
  args: { onRemove: fn() },
  render: (args) => (
    <div style={{ display: "flex", gap: "1rem", alignItems: "center" }}>
      <Tag size="sm" removable onRemove={args.onRemove} data-testid="tag-sm">
        small
      </Tag>
      <Tag size="md" removable onRemove={args.onRemove} data-testid="tag-md">
        medium
      </Tag>
    </div>
  ),
};
