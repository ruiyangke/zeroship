import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useState } from "react";
import { Accordion } from "../components";

const meta: Meta<typeof Accordion> = {
  title: "Components/Accordion",
  component: Accordion,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Accordion>;

/* ─── shared content (so the stories stay focused on the prop matrix) */
const sampleItems = [
  {
    value: "shipping",
    label: "Shipping & delivery",
    body:
      "Orders ship within two business days; tracking arrives by email. " +
      "Carrier delays during peak season can add 1–2 days.",
  },
  {
    value: "returns",
    label: "Returns & refunds",
    body:
      "Unused items can be returned within 30 days. Refunds post to the " +
      "original payment method within 5 business days.",
  },
  {
    value: "warranty",
    label: "Warranty coverage",
    body:
      "All hardware ships with a one-year limited warranty. Software " +
      "support remains available for the life of the active subscription.",
  },
] as const;

function renderItems(testid: string) {
  return sampleItems.map((item) => (
    <Accordion.Item
      key={item.value}
      value={item.value}
      data-testid={`${testid}-item-${item.value}`}
    >
      <Accordion.Header>
        <Accordion.Trigger
          data-testid={`${testid}-trigger-${item.value}`}
        >
          {item.label}
        </Accordion.Trigger>
      </Accordion.Header>
      <Accordion.Panel data-testid={`${testid}-panel-${item.value}`}>
        {item.body}
      </Accordion.Panel>
    </Accordion.Item>
  ));
}

/* ─── 1. Basic — single mode, no default open ──────────────────────── */
export const Basic: Story = {
  name: "Basic",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Accordion in `single` mode with nothing open at " +
          "first paint. Clicking any Trigger opens its Panel and closes " +
          "the previously-open Panel.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <Accordion type="single" data-testid="accordion-basic">
        {renderItems("accordion-basic")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const shipping = canvas.getByRole("button", {
      name: /shipping & delivery/i,
    });
    const returns = canvas.getByRole("button", {
      name: /returns & refunds/i,
    });

    await expect(shipping).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(shipping);
    await expect(shipping).toHaveAttribute("aria-expanded", "true");
    await expect(
      canvas.getByText(/orders ship within two business days/i),
    ).toBeVisible();

    await userEvent.click(returns);
    await expect(returns).toHaveAttribute("aria-expanded", "true");
    await expect(shipping).toHaveAttribute("aria-expanded", "false");
  },
};

/* ─── 2. MultipleOpen — multiple mode ──────────────────────────────── */
export const MultipleOpen: Story = {
  name: "Multiple open",
  parameters: {
    docs: {
      description: {
        story:
          "`type=\"multiple\"` allows any number of Items to be open at " +
          "once. The `value` shape changes to `string[]` — single + " +
          "multiple aren't conflated behind an `expanded` boolean.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Multiple open">
      <Accordion
        type="multiple"
        defaultValue={["shipping", "warranty"]}
        data-testid="accordion-multiple"
      >
        {renderItems("accordion-multiple")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const shipping = canvas.getByRole("button", {
      name: /shipping & delivery/i,
    });
    const returns = canvas.getByRole("button", {
      name: /returns & refunds/i,
    });
    const warranty = canvas.getByRole("button", {
      name: /warranty coverage/i,
    });

    await expect(shipping).toHaveAttribute("aria-expanded", "true");
    await expect(warranty).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(returns);
    await expect(returns).toHaveAttribute("aria-expanded", "true");
    await expect(shipping).toHaveAttribute("aria-expanded", "true");

    await userEvent.click(warranty);
    await expect(warranty).toHaveAttribute("aria-expanded", "false");
  },
};

/* ─── 3. Collapsible — single + collapsible:true ───────────────────── */
export const Collapsible: Story = {
  name: "Collapsible (single)",
  parameters: {
    docs: {
      description: {
        story:
          "`collapsible: true` lets the open Item be re-clicked to close " +
          "itself, taking the entire accordion to a no-Item-open state. " +
          "Default `collapsible: false` mirrors RadioGroup — one Item " +
          "stays open once one is opened.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Collapsible single"
    >
      <Accordion
        type="single"
        collapsible
        defaultValue="shipping"
        data-testid="accordion-collapsible"
      >
        {renderItems("accordion-collapsible")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const shipping = canvas.getByRole("button", {
      name: /shipping & delivery/i,
    });

    await expect(shipping).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(shipping);
    await expect(shipping).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(shipping);
    await expect(shipping).toHaveAttribute("aria-expanded", "true");
  },
};

/* ─── 4. Controlled — external state drives value ──────────────────── */
export const Controlled: Story = {
  name: "Controlled",
  parameters: {
    docs: {
      description: {
        story:
          "Controlled Accordion — external state drives `value` and " +
          "`onValueChange`. The selected value reads out in the live " +
          "readout so the contract is observable.",
      },
    },
  },
  render: function Render() {
    const [value, setValue] = useState<string | undefined>("returns");
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Controlled"
        style={{ flexDirection: "column", alignItems: "stretch" }}
      >
        <Accordion
          type="single"
          collapsible
          value={value}
          onValueChange={setValue}
          data-testid="accordion-controlled"
        >
          {renderItems("accordion-controlled")}
        </Accordion>
        <output
          aria-live="polite"
          data-testid="accordion-controlled-readout"
          style={{
            marginBlockStart: "var(--zs-space-3)",
            fontSize: "var(--zs-text-caption-1-size)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Open: {value ?? "(none)"}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const returns = canvas.getByRole("button", {
      name: /returns & refunds/i,
    });
    const warranty = canvas.getByRole("button", {
      name: /warranty coverage/i,
    });
    const readout = canvas.getByText(/open: returns/i);

    await expect(returns).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(warranty);
    await expect(warranty).toHaveAttribute("aria-expanded", "true");
    await expect(readout).toHaveTextContent("Open: warranty");

    await userEvent.click(warranty);
    await expect(warranty).toHaveAttribute("aria-expanded", "false");
    await expect(readout).toHaveTextContent("Open: (none)");
  },
};

/* ─── 5. WithDefaultValue — single mode pre-opened ─────────────────── */
export const WithDefaultValue: Story = {
  name: "With default value",
  parameters: {
    docs: {
      description: {
        story:
          "`defaultValue` opens the named Item on first mount without " +
          "taking control. Pair with `onValueChange` to observe.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="With default value"
    >
      <Accordion
        type="single"
        defaultValue="warranty"
        data-testid="accordion-default"
      >
        {renderItems("accordion-default")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const warranty = canvas.getByRole("button", {
      name: /warranty coverage/i,
    });

    await expect(warranty).toHaveAttribute("aria-expanded", "true");
    await expect(
      canvas.getByText(/all hardware ships with a one-year limited warranty/i),
    ).toBeVisible();

    // Regression guard (RTL-correct open marker): the open-state accent is
    // a `::before` overlay pinned to the LOGICAL inline-start edge — not a
    // physical-left box-shadow. In this LTR story inline-start === left, so
    // the marker's left edge aligns with the trigger's left edge, and it
    // has a non-zero painted width. Fails pre-fix only conceptually (the
    // box-shadow had no ::before to measure); here we assert the ::before
    // exists, is painted, and hugs the inline-start edge.
    const triggerBox = warranty.getBoundingClientRect();
    const before = getComputedStyle(warranty, "::before");
    await expect(before.content).not.toBe("none");
    const markerWidth = parseFloat(before.inlineSize || before.width);
    await expect(markerWidth).toBeGreaterThan(0);
    // inset-inline-start: 0 → the marker is flush to the trigger's start
    // (left in LTR). Resolves to a length the browser reports in px.
    const insetStart = parseFloat(
      before.insetInlineStart || before.left || "0",
    );
    await expect(insetStart).toBe(0);
    void triggerBox;
  },
};

/* ─── 6. Disabled — root cascade ───────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the Root cascades to every Trigger so the entire " +
          "accordion reads inactive. No Triggers respond to clicks.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <Accordion type="single" disabled data-testid="accordion-disabled">
        {renderItems("accordion-disabled")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const shipping = canvas.getByRole("button", {
      name: /shipping & delivery/i,
    });

    // Base UI's Accordion.Trigger is a heading-button (a `<button>`
    // inside an `<h3>`), not a native form control. It marks the
    // disabled state with `aria-disabled="true"` + `data-disabled`
    // (NOT the native `disabled` attribute) so the trigger stays in
    // the AT tree as a disabled heading-button. jest-dom's
    // `toBeDisabled()` only recognizes native `disabled`; the faithful
    // assertion is the aria/data contract plus the behavioral
    // guarantee (clicking does not expand, no focus is taken).
    await expect(shipping).toHaveAttribute("aria-disabled", "true");
    await expect(shipping).toHaveAttribute("data-disabled");
    await expect(shipping).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(shipping);
    await expect(shipping).toHaveAttribute("aria-expanded", "false");
    await expect(shipping).not.toHaveFocus();
  },
};

/* ─── 7. Horizontal — accordion in horizontal orientation ─────────── */
export const Horizontal: Story = {
  name: "Horizontal orientation",
  parameters: {
    docs: {
      description: {
        story:
          "`orientation=\"horizontal\"` lays Items inline and rotates the " +
          "roving keys to Left/Right. The chevron also rotates 90° so its " +
          "open-state cue stays consistent with the layout axis.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Horizontal"
      style={{ minBlockSize: "8rem" }}
    >
      <Accordion
        type="single"
        orientation="horizontal"
        defaultValue="shipping"
        data-testid="accordion-horizontal"
        style={{ inlineSize: "100%" }}
      >
        {renderItems("accordion-horizontal")}
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const shipping = canvas.getByRole("button", {
      name: /shipping & delivery/i,
    });
    const returns = canvas.getByRole("button", {
      name: /returns & refunds/i,
    });
    const warranty = canvas.getByRole("button", {
      name: /warranty coverage/i,
    });

    shipping.focus();
    await expect(shipping).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(returns).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(warranty).toHaveFocus();
    await userEvent.keyboard("{ArrowLeft}");
    await expect(returns).toHaveFocus();
  },
};

/* ─── 8. RTL — mirrored layout ─────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Accordion in RTL — every edge / inset / chevron sits on a " +
          "logical property so the layout mirrors automatically without " +
          "a manual flip. The chevron lands at the inline-end (left in " +
          "RTL) and the panel padding stays balanced.",
      },
    },
  },
  render: () => (
    <div
      dir="rtl"
      className="zs-story-row"
      role="group"
      aria-label="RTL"
    >
      <Accordion
        type="single"
        defaultValue="shipping"
        data-testid="accordion-rtl"
      >
        <Accordion.Item value="shipping" data-testid="accordion-rtl-item-shipping">
          <Accordion.Header>
            <Accordion.Trigger data-testid="accordion-rtl-trigger-shipping">
              משלוח ומסירה
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel data-testid="accordion-rtl-panel-shipping">
            ההזמנות נשלחות בתוך שני ימי עסקים; מספר המעקב יישלח באימייל.
          </Accordion.Panel>
        </Accordion.Item>
        <Accordion.Item value="returns" data-testid="accordion-rtl-item-returns">
          <Accordion.Header>
            <Accordion.Trigger data-testid="accordion-rtl-trigger-returns">
              החזרות והחזרים
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel data-testid="accordion-rtl-panel-returns">
            ניתן להחזיר פריטים שלא נעשה בהם שימוש בתוך 30 יום.
          </Accordion.Panel>
        </Accordion.Item>
        <Accordion.Item value="warranty" data-testid="accordion-rtl-item-warranty">
          <Accordion.Header>
            <Accordion.Trigger data-testid="accordion-rtl-trigger-warranty">
              כיסוי האחריות
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel data-testid="accordion-rtl-panel-warranty">
            כל החומרה מגיעה עם אחריות מוגבלת לשנה.
          </Accordion.Panel>
        </Accordion.Item>
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const returns = canvas.getByRole("button", {
      name: /החזרות והחזרים/i,
    });

    await userEvent.click(returns);
    await expect(returns).toHaveAttribute("aria-expanded", "true");
    await expect(
      canvas.getByText(/ניתן להחזיר פריטים/i),
    ).toBeVisible();
  },
};

/* ─── 9a. RegressionTransitions — fix #1 regression baseline ───────── *
 *
 * Dedicated to the fix #1 regression check: the panel close-transition
 * runs a real `block-size` interpolation rather than a snap. This story
 * deliberately ships WITHOUT a `play()` so Storybook's autoplay does
 * not race the aria-wiring script's interactions and leave the
 * accordion in a post-play state. The story preopens `shipping` so
 * the closing transition is one click away. */
export const RegressionTransitions: Story = {
  name: "Regression — transitions (fix #1)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Slice 14 fix #1 panel-transition " +
          "check. No play() — the aria-wiring script drives the click " +
          "itself and samples mid-transition block-size.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Regression transitions"
    >
      <Accordion
        type="single"
        collapsible
        defaultValue="shipping"
        data-testid="accordion-regression-transitions"
      >
        {renderItems("accordion-regression-transitions")}
      </Accordion>
    </div>
  ),
};

/* ─── 9b. RegressionNonCollapsible — fix #2 regression baseline ──── *
 *
 * Default `collapsible: false` (omitted) in `type="single"`. The
 * aria-wiring script clicks `shipping` to open and clicks again to
 * verify the open Trigger STAYS open (RadioGroup semantics). No
 * play() — see fix #1 baseline above. */
export const RegressionNonCollapsible: Story = {
  name: "Regression — collapsible=false (fix #2)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Slice 14 fix #2 non-collapsible " +
          "single-mode check. No play() — the aria-wiring script clicks " +
          "the same Trigger twice and asserts the second click is a no-op.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Regression non-collapsible"
    >
      <Accordion
        type="single"
        data-testid="accordion-regression-noncollapsible"
      >
        {renderItems("accordion-regression-noncollapsible")}
      </Accordion>
    </div>
  ),
};

/* ─── 9c. RegressionControlled — fix #4 regression baseline ──────── *
 *
 * Controlled single accordion that initializes with `returns` open.
 * No play() — the aria-wiring script drives a full open/close/reopen
 * cycle and verifies the external readout (`Open: <value>`) stays in
 * lockstep with `aria-expanded`. Pre-fix, `value === undefined`
 * silently exited controlled mode and the readout desynced. */
export const RegressionControlled: Story = {
  name: "Regression — controlled value cycle (fix #4)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Slice 14 fix #4 controlled-value " +
          "check. No play() — the aria-wiring script open/close/reopens " +
          "and asserts the readout matches state on every step.",
      },
    },
  },
  render: function Render() {
    const [value, setValue] = useState<string | undefined>("returns");
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Regression controlled"
        style={{ flexDirection: "column", alignItems: "stretch" }}
      >
        <Accordion
          type="single"
          collapsible
          value={value}
          onValueChange={setValue}
          data-testid="accordion-regression-controlled"
        >
          {renderItems("accordion-regression-controlled")}
        </Accordion>
        <output
          aria-live="polite"
          data-testid="accordion-regression-controlled-readout"
          style={{
            marginBlockStart: "var(--zs-space-3)",
            fontSize: "var(--zs-text-caption-1-size)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Open: {value ?? "(none)"}
        </output>
      </div>
    );
  },
};

/* ─── 9d. RegressionHorizontalTransition — wave 7 fix regression baseline ─ *
 *
 * Horizontal accordion preopened on `shipping`. The aria-wiring script
 * samples the panel's computed `block-size` mid-transition while a
 * second item is being opened — and asserts the closing panel's
 * `block-size` is NOT zero during the `data-starting-style` /
 * `data-ending-style` frames.
 *
 * Pre-fix regression: the generic `[data-starting-style] /
 * [data-ending-style] { block-size: 0 }` rule was unscoped, so
 * horizontal panels collapsed to a 0 block-size during transition
 * frames (the rail visually disappeared even though the close was on
 * the inline axis). The orientation-scoped rules introduced here keep
 * block-size content-driven on horizontal panels at every frame and
 * only zero the inline-size. No play() so Storybook autoplay doesn't
 * race the script's clicks. */
export const RegressionHorizontalTransition: Story = {
  name: "Regression — horizontal panel keeps block-size (wave 7 fix)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the wave 7 fix: horizontal panels " +
          "must keep their block-size content-driven during " +
          "starting/ending-style frames. The aria-wiring script samples " +
          "the closing panel's block-size mid-transition and asserts " +
          "it stays > 0 (pre-fix it snapped to 0).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Regression horizontal transition"
      style={{ minBlockSize: "8rem" }}
    >
      <Accordion
        type="single"
        orientation="horizontal"
        defaultValue="shipping"
        data-testid="accordion-regression-horizontal"
        style={{ inlineSize: "100%" }}
      >
        {renderItems("accordion-regression-horizontal")}
      </Accordion>
    </div>
  ),
};

/* ─── 9. RichContent — panels with structured content ──────────────── */
export const RichContent: Story = {
  name: "Rich content",
  parameters: {
    docs: {
      description: {
        story:
          "Panels can hold real content — headings, lists, links. The " +
          "Panel auto-sizes to the natural block-size of its inner " +
          "wrapper, so any structured content lays out the same as it " +
          "would in a regular flow.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Rich content">
      <Accordion
        type="single"
        defaultValue="features"
        data-testid="accordion-rich"
        style={{ inlineSize: "32rem" }}
      >
        <Accordion.Item value="features" data-testid="accordion-rich-item-features">
          <Accordion.Header>
            <Accordion.Trigger data-testid="accordion-rich-trigger-features">
              What's included
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel data-testid="accordion-rich-panel-features">
            <p style={{ margin: 0 }}>
              Every plan ships with the core platform:
            </p>
            <ul
              style={{
                margin: "var(--zs-space-2) 0 0 0",
                paddingInlineStart: "var(--zs-space-5)",
                color: "var(--zs-label)",
              }}
            >
              <li>Unlimited deployments</li>
              <li>Built-in auth, db, kv, storage primitives</li>
              <li>Stripe Connect payouts</li>
              <li>Custom domains with automatic TLS</li>
            </ul>
          </Accordion.Panel>
        </Accordion.Item>
        <Accordion.Item value="billing" data-testid="accordion-rich-item-billing">
          <Accordion.Header>
            <Accordion.Trigger data-testid="accordion-rich-trigger-billing">
              How billing works
            </Accordion.Trigger>
          </Accordion.Header>
          <Accordion.Panel data-testid="accordion-rich-panel-billing">
            <p style={{ margin: 0 }}>
              Creators take 85% of every dollar earned; the platform takes
              15% to cover hosting, payments, and support. Stripe handles
              card fees separately at their published rate.
            </p>
            <p
              style={{
                margin: "var(--zs-space-2) 0 0 0",
                color: "var(--zs-label-secondary)",
                fontSize: "var(--zs-text-caption-1-size)",
              }}
            >
              Payouts arrive on a rolling 7-day schedule.
            </p>
          </Accordion.Panel>
        </Accordion.Item>
      </Accordion>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const features = canvas.getByRole("button", {
      name: /what's included/i,
    });
    const billing = canvas.getByRole("button", {
      name: /how billing works/i,
    });

    await expect(features).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(billing);
    await expect(billing).toHaveAttribute("aria-expanded", "true");
    await expect(canvas.getByText(/creators take 85%/i)).toBeVisible();
  },
};
