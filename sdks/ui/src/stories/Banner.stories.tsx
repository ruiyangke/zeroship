import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { Banner, type BannerIntent } from "../blocks";
import { Button } from "../components";

const meta: Meta<typeof Banner> = {
  title: "Blocks/Banner",
  component: Banner,
  parameters: { layout: "fullscreen" },
  argTypes: {
    intent: {
      control: "inline-radio",
      options: ["info", "success", "warning", "danger"],
    },
    dismissible: { control: "boolean" },
    live: { control: "boolean" },
  },
};

export default meta;

type Story = StoryObj<typeof Banner>;

const INTENTS: BannerIntent[] = ["info", "success", "warning", "danger"];

/* ─── 1. Every intent ─────────────────────────────────────────────────── */
export const Intents: Story = {
  name: "Intents (info / success / warning / danger)",
  parameters: {
    docs: {
      description: {
        story:
          "The four intents tint the opaque shell + the leading icon. " +
          "Each shell paints an opaque tinted background (intent blended " +
          "INTO the surface, never alpha-over-page) so axe's contrast " +
          "walk resolves. None of these are live regions (static render).",
      },
    },
  },
  render: () => (
    <div style={{ display: "grid", gap: "var(--zs-space-3)" }}>
      {INTENTS.map((intent) => (
        <Banner
          key={intent}
          intent={intent}
          title={`This is an ${intent} banner`}
          description="A short supporting line that explains the message."
        />
      ))}
    </div>
  ),
};

/* ─── 2. Dismissible (play: click → onDismiss) ───────────────────────── */
export const Dismissible: Story = {
  name: "Dismissible",
  parameters: {
    docs: {
      description: {
        story:
          "`dismissible` renders a real `<button aria-label=\"Dismiss\">` " +
          "(× glyph aria-hidden). The play() clicks it and asserts the " +
          "`onDismiss` spy fires. Banner does not hide itself — the " +
          "consumer removes it from the tree.",
      },
    },
  },
  args: { onDismiss: fn() },
  render: (args) => (
    <Banner
      data-testid="banner-dismiss"
      intent="info"
      dismissible
      onDismiss={args.onDismiss}
      title="Heads up"
      description="You can dismiss this message."
    />
  ),
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    const dismiss = canvas.getByRole("button", { name: /dismiss/i });
    await userEvent.click(dismiss);
    await expect(args.onDismiss).toHaveBeenCalledTimes(1);
  },
};

/* ─── 3. With actions (compound parts) ───────────────────────────────── */
export const WithActions: Story = {
  name: "With actions (compound parts)",
  parameters: {
    docs: {
      description: {
        story:
          "Built from compound parts — `Banner.Title` / `.Description` / " +
          "`.Actions`. Use ONE mode: compound parts OR ergonomic props.",
      },
    },
  },
  render: () => (
    <Banner intent="warning" dismissible onDismiss={() => {}}>
      <Banner.Title>Your trial ends in 3 days</Banner.Title>
      <Banner.Description>
        Upgrade now to keep your projects and avoid interruption.
      </Banner.Description>
      <Banner.Actions>
        <Button variant="filled" size="small">
          Upgrade
        </Button>
        {/* `tinted`/`gray` (not `plain`) on a tinted banner: a plain
            button's transparent fill leaves its accent ink resolving
            against the orange banner tint, which fails contrast. A
            button that carries its own opaque background sidesteps that. */}
        <Button variant="gray" size="small">
          Remind me later
        </Button>
      </Banner.Actions>
    </Banner>
  ),
};

/* ─── 4. Live (info → role=status) ───────────────────────────────────── */
export const LiveStatus: Story = {
  name: "Live — info (role=status)",
  parameters: {
    docs: {
      description: {
        story:
          "Set `live` when the banner appears dynamically. info/success " +
          "become `role=\"status\"` (polite). Static banners must stay a " +
          "plain region so the SR isn't interrupted.",
      },
    },
  },
  render: () => (
    <Banner
      data-testid="banner-live-status"
      live
      intent="success"
      title="Changes saved"
      description="Your edits are live."
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const status = canvas.getByRole("status");
    await expect(status).toHaveAttribute("data-testid", "banner-live-status");
  },
};

/* ─── 5. Live (danger → role=alert) ──────────────────────────────────── */
export const LiveAlert: Story = {
  name: "Live — danger (role=alert)",
  parameters: {
    docs: {
      description: {
        story:
          "warning/danger live banners become `role=\"alert\"` " +
          "(assertive) so the SR interrupts to announce the failure.",
      },
    },
  },
  render: () => (
    <Banner
      data-testid="banner-live-alert"
      live
      intent="danger"
      title="Connection lost"
      description="We couldn't reach the server. Retrying…"
    />
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const alert = canvas.getByRole("alert");
    await expect(alert).toHaveAttribute("data-testid", "banner-live-alert");
  },
};
