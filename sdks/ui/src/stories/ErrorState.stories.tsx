import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { ErrorState } from "../blocks";

const meta: Meta<typeof ErrorState> = {
  title: "Blocks/ErrorState",
  component: ErrorState,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof ErrorState>;

/* ─── 1. Default (error) with Retry ─────────────────────────────────── */
export const WithRetry: Story = {
  name: "Error + Retry",
  parameters: {
    docs: {
      description: {
        story:
          "Default `intent='error'` (red icon). Passing `onRetry` renders " +
          "a real `Retry` Button. The play() clicks it and asserts the " +
          "handler fires. Statically rendered → NOT a live region " +
          "(`live` is false), so there is no `role='alert'`.",
      },
    },
  },
  args: {
    onRetry: fn(),
  },
  render: (args) => (
    <ErrorState
      data-testid="error-retry"
      title="Couldn't load your projects"
      description="Something went wrong on our end. Check your connection and try again."
      onRetry={args.onRetry}
    />
  ),
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("error-retry");
    await expect(root).toHaveAttribute("data-intent", "error");
    // Not a live region by default.
    await expect(canvas.queryByRole("alert")).not.toBeInTheDocument();

    const heading = canvas.getByRole("heading", {
      name: /couldn't load your projects/i,
    });
    await expect(heading.tagName).toBe("H2");

    const retry = canvas.getByRole("button", { name: /retry/i });
    await userEvent.click(retry);
    await expect(args.onRetry).toHaveBeenCalledTimes(1);
  },
};

/* ─── 2. Live (role=alert) ──────────────────────────────────────────── */
export const Live: Story = {
  name: "Live (role=alert)",
  parameters: {
    docs: {
      description: {
        story:
          "Set `live` when the error appears dynamically (a failed save, " +
          "a dropped connection) so screen readers announce it. The root " +
          "becomes `role='alert'`. Do NOT set `live` on a statically " +
          "rendered error route — it would interrupt the user.",
      },
    },
  },
  render: () => (
    <ErrorState
      data-testid="error-live"
      live
      title="Save failed"
      description="We couldn't save your changes. They're still here — try again."
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const alert = canvas.getByRole("alert");
    await expect(alert).toHaveAttribute("data-testid", "error-live");
    await expect(
      within(alert).getByRole("heading", { name: /save failed/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 3. Warning intent (compound) ──────────────────────────────────── */
export const WarningCompound: Story = {
  name: "Warning intent (compound parts)",
  parameters: {
    docs: {
      description: {
        story:
          "`intent='warning'` tints the icon orange. Built from compound " +
          "parts — `ErrorState.Title` / `.Description` / `.Actions`.",
      },
    },
  },
  render: () => (
    <ErrorState data-testid="error-warning" intent="warning">
      <ErrorState.Title>Storage almost full</ErrorState.Title>
      <ErrorState.Description>
        You've used 92% of your quota. Free up space to keep uploading.
      </ErrorState.Description>
    </ErrorState>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("error-warning");
    await expect(root).toHaveAttribute("data-intent", "warning");
    await expect(
      canvas.getByRole("heading", { name: /storage almost full/i }),
    ).toBeInTheDocument();
  },
};
