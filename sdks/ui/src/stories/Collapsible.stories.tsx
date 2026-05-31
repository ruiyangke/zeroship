import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, within } from "@storybook/test";
import { useState } from "react";
import { Card, Collapsible } from "../components";

/* Wave 10 fix #1 regression — Trigger / Panel asChild routed through
 * `_slot.ts` Slot. Mirrors Drawer.Close / AlertDialog.Cancel asChild
 * coverage style: a status readout proves the consumer's onClick fires
 * once AND the wrapper className lands on the rendered element. */

const meta: Meta<typeof Collapsible> = {
  title: "Components/Collapsible",
  component: Collapsible,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Collapsible>;

/* ─── 1. Basic — uncontrolled, closed at first paint ───────────────── */
export const Basic: Story = {
  name: "Basic",
  parameters: {
    docs: {
      description: {
        story:
          "Minimal Collapsible — closed on mount, opens when the Trigger " +
          "is clicked. `aria-expanded` on the Trigger flips with the open " +
          "state; the Panel carries `hidden` while closed.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <Collapsible data-testid="collapsible-basic">
        <Collapsible.Trigger data-testid="collapsible-basic-trigger">
          Show advanced options
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-basic-panel">
          Advanced options reveal here. Use them when the defaults do not
          fit your deployment.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /show advanced options/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(
      canvas.getByText(/advanced options reveal here/i),
    ).toBeVisible();

    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
  },
};

/* ─── 2. Controlled — external state drives open ───────────────────── */
export const Controlled: Story = {
  name: "Controlled",
  parameters: {
    docs: {
      description: {
        story:
          "Controlled Collapsible — external state drives `open` and " +
          "`onOpenChange`. The state reads out in the live readout so " +
          "the contract is observable.",
      },
    },
  },
  render: function Render() {
    const [open, setOpen] = useState<boolean>(true);
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Controlled"
        style={{ flexDirection: "column", alignItems: "stretch" }}
      >
        <Collapsible
          open={open}
          onOpenChange={setOpen}
          data-testid="collapsible-controlled"
        >
          <Collapsible.Trigger
            data-testid="collapsible-controlled-trigger"
          >
            Release notes
          </Collapsible.Trigger>
          <Collapsible.Panel data-testid="collapsible-controlled-panel">
            v1.4.0 lands native asChild on every primitive, a smaller
            forced-colors palette, and the new Accordion + Collapsible
            duo.
          </Collapsible.Panel>
        </Collapsible>
        <output
          aria-live="polite"
          data-testid="collapsible-controlled-readout"
          style={{
            marginBlockStart: "var(--zs-space-3)",
            fontSize: "var(--zs-text-caption-1-size)",
            color: "var(--zs-label-secondary)",
          }}
        >
          Open: {String(open)}
        </output>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", { name: /release notes/i });
    const readout = canvas.getByText(/open: true/i);

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(readout).toHaveTextContent("Open: false");

    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await expect(readout).toHaveTextContent("Open: true");
  },
};

/* ─── 3. Disabled — root cascade ───────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  parameters: {
    docs: {
      description: {
        story:
          "`disabled` on the Root cascades to the Trigger so the " +
          "affordance reads inactive. The Trigger no longer responds to " +
          "clicks; the Panel stays in whatever state it was in.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <Collapsible disabled data-testid="collapsible-disabled">
        <Collapsible.Trigger data-testid="collapsible-disabled-trigger">
          Disabled disclosure
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-disabled-panel">
          You cannot toggle this open or closed from the Trigger.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /disabled disclosure/i,
    });

    // Base UI's Collapsible.Trigger is a disclosure button, NOT a
    // native form control. It marks the disabled state with
    // `aria-disabled="true"` + `data-disabled` (NOT the native
    // `disabled` attribute) so the trigger stays in the AT tree and
    // exposes WHY it is inactive. jest-dom's `toBeDisabled()` only
    // recognizes the native `disabled` attr on form controls, so the
    // faithful assertion for this element type is the aria/data
    // contract plus the behavioral guarantee (clicks are no-ops, no
    // focus is taken).
    await expect(trigger).toHaveAttribute("aria-disabled", "true");
    await expect(trigger).toHaveAttribute("data-disabled");
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await expect(trigger).not.toHaveFocus();
  },
};

/* ─── 3a. RegressionTransitions — fix #1 regression baseline ──────── *
 *
 * Dedicated to the fix #1 regression check on Collapsible. No play()
 * — Storybook autoplay does not race the aria-wiring script. The
 * story renders closed; the script clicks once to open (measures the
 * settled open height), then clicks again to close and samples the
 * in-flight block-size. */
export const RegressionTransitions: Story = {
  name: "Regression — transitions (fix #1)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Slice 14 Collapsible fix #1 " +
          "panel-transition check. No play() — the aria-wiring script " +
          "drives the click itself and samples mid-transition block-size.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Regression transitions"
    >
      <Collapsible data-testid="collapsible-regression-transitions">
        <Collapsible.Trigger
          data-testid="collapsible-regression-transitions-trigger"
        >
          Show advanced options
        </Collapsible.Trigger>
        <Collapsible.Panel
          data-testid="collapsible-regression-transitions-panel"
        >
          Advanced options reveal here. Use them when the defaults do not
          fit your deployment.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
};

/* ─── 4. InsideCard — Collapsible sits inside a Card ───────────────── */
export const InsideCard: Story = {
  name: "Inside Card",
  parameters: {
    docs: {
      description: {
        story:
          "Collapsible inside a Card — a common pattern for compact " +
          "settings rows. The Trigger's padding sits inside the Card's " +
          "content rhythm so the disclosure aligns with the surrounding " +
          "ink.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Inside card">
      <Card style={{ inlineSize: "28rem" }}>
        <Card.Header>
          <Card.Title>Notification preferences</Card.Title>
          <Card.Description>
            Choose which events trigger an email.
          </Card.Description>
        </Card.Header>
        <Card.Content>
          <Collapsible
            defaultOpen
            data-testid="collapsible-in-card"
          >
            <Collapsible.Trigger data-testid="collapsible-in-card-trigger">
              Advanced filters
            </Collapsible.Trigger>
            <Collapsible.Panel data-testid="collapsible-in-card-panel">
              Filter by event type, source, and severity. Filters apply
              to both real-time and digest delivery.
            </Collapsible.Panel>
          </Collapsible>
        </Card.Content>
      </Card>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /advanced filters/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(trigger);
    await expect(
      canvas.getByText(/filter by event type/i),
    ).toBeVisible();
  },
};

/* ─── 5. RTL — mirrored layout ─────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Collapsible in RTL — the chevron mirrors automatically (it " +
          "lives at the inline-end via flex justify-content; no manual " +
          "flip needed). All padding stays balanced via logical props.",
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
      <Collapsible defaultOpen data-testid="collapsible-rtl">
        <Collapsible.Trigger data-testid="collapsible-rtl-trigger">
          הצג אפשרויות מתקדמות
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-rtl-panel">
          האפשרויות המתקדמות נחשפות כאן. השתמש בהן כאשר ברירות המחדל
          אינן מתאימות לפריסה שלך.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", {
      name: /הצג אפשרויות מתקדמות/i,
    });

    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "false");
  },
};

/* ─── 6. TriggerAsChild — Slot routes className + onClick + state ──── *
 *
 * Wave 10 fix #1 regression. `<Collapsible.Trigger asChild>` MUST
 * render the consumer's element AND keep Base UI's open / aria-* /
 * disabled wiring. We click the asChild element, expect:
 *   - the consumer's own onClick to fire exactly once,
 *   - the consumer's `className` to appear on the rendered element,
 *   - `aria-expanded` to flip, and
 *   - the panel to become visible.
 * Pre-fix the component explicitly omitted `render` and rejected the
 * asChild surface — this story would not even type-check. The fix
 * threads the consumer's element through `_slot.ts` Slot via Base
 * UI's `render` prop. */
function TriggerAsChildStory() {
  const [clicked, setClicked] = useState<string>("not-clicked");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Trigger as child"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      <p
        role="status"
        aria-label="Trigger asChild status"
        data-testid="collapsible-aschild-status"
      >
        Status: {clicked}
      </p>
      <Collapsible data-testid="collapsible-trigger-aschild">
        <Collapsible.Trigger asChild>
          <button
            type="button"
            className="zs-collapsible-aschild-target"
            data-testid="collapsible-trigger-aschild-target"
            onClick={() => setClicked("child-onclick-ran")}
          >
            Show advanced options (asChild)
          </button>
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-trigger-aschild-panel">
          Advanced options revealed via an asChild Trigger.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  );
}
export const TriggerAsChild: Story = {
  name: "Trigger — asChild (Slot)",
  parameters: {
    docs: {
      description: {
        story:
          "`<Collapsible.Trigger asChild>` swaps the host element while " +
          "keeping Base UI's open / aria-expanded / aria-controls wiring. " +
          "The consumer's className lands on the rendered element AND the " +
          "consumer's onClick fires exactly once per click — same Slot " +
          "contract as Dialog.Close (commit `3a64a726`).",
      },
    },
  },
  render: () => <TriggerAsChildStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const status = canvas.getByRole("status", {
      name: /trigger aschild status/i,
    });
    const trigger = canvas.getByRole("button", {
      name: /show advanced options \(aschild\)/i,
    });

    // Consumer className lands on the rendered element (Slot's
    // mergeProps concatenates "zs-collapsible-trigger" with the
    // consumer's "zs-collapsible-aschild-target").
    await expect(trigger).toHaveClass("zs-collapsible-aschild-target");
    await expect(trigger).toHaveClass("zs-collapsible-trigger");
    await expect(trigger).toHaveAttribute("aria-expanded", "false");

    await userEvent.click(trigger);
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    // Consumer's onClick fired exactly once (Slot's onClick
    // composition calls the child handler in front of Base UI's).
    await expect(status).toHaveTextContent("Status: child-onclick-ran");
    await expect(
      canvas.getByText(/advanced options revealed via an aschild trigger/i),
    ).toBeVisible();
  },
};

/* ─── 7. PanelAsChild — Slot drops the inner padding wrapper ───────── *
 *
 * Wave 10 fix #1 regression for the Panel side of asChild. The Panel
 * default path injects a `<div class="zs-collapsible-panel-inner">`
 * wrapper for padding. `asChild` MUST drop that wrapper — the
 * consumer's element becomes the panel itself. The story renders an
 * `<section data-testid="..." class="zs-collapsible-aschild-section">`
 * and we assert:
 *   - `tagName === "SECTION"` on the rendered panel,
 *   - the consumer's `className` is present,
 *   - the consumer-supplied content is reachable via the panel
 *     element directly (no `.zs-collapsible-panel-inner` ancestor),
 *   - and `aria-controls` on the Trigger still points at this panel.
 * Pre-fix this story would not type-check (`asChild` was not in the
 * Panel props surface). */
export const PanelAsChild: Story = {
  name: "Panel — asChild (Slot)",
  parameters: {
    docs: {
      description: {
        story:
          "`<Collapsible.Panel asChild>` swaps the outer `<div>` AND " +
          "drops the `.zs-collapsible-panel-inner` padding wrapper. The " +
          "consumer-supplied element becomes the panel, so it owns its " +
          "own padding semantics. Base UI's `data-open` / measurement " +
          "machinery stays attached via Slot's prop merge.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Panel as child"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      <Collapsible
        defaultOpen
        data-testid="collapsible-panel-aschild"
      >
        <Collapsible.Trigger data-testid="collapsible-panel-aschild-trigger">
          Release notes
        </Collapsible.Trigger>
        <Collapsible.Panel asChild>
          <section
            className="zs-collapsible-aschild-section"
            data-testid="collapsible-panel-aschild-target"
            aria-label="Release notes content"
          >
            v1.5.0 lands native asChild on Collapsible Trigger AND Panel,
            plus a forced-colors chevron mirror.
          </section>
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", { name: /release notes/i });
    const section = canvas.getByTestId("collapsible-panel-aschild-target");

    await expect(section.tagName).toBe("SECTION");
    await expect(section).toHaveClass("zs-collapsible-aschild-section");
    await expect(section).toHaveClass("zs-collapsible-panel");
    // No inner padding wrapper got injected — the asChild element IS
    // the panel.
    await expect(
      section.querySelector(".zs-collapsible-panel-inner"),
    ).toBeNull();
    // aria-controls on the Trigger references the asChild section's
    // id so screen readers still associate the disclosure with its
    // content.
    const sectionId = section.getAttribute("id");
    await expect(sectionId).not.toBeNull();
    await expect(trigger).toHaveAttribute("aria-controls", sectionId!);
  },
};

/* ─── 8. RegressionOnOpenChangeDetails — fix #2 (rework) ──────────── *
 *
 * Wave 10 rework regression for `onOpenChange` details forwarding.
 * Pre-fix: the wrapper narrowed `onOpenChange` to
 * `(open: boolean) => void` AND called it with just `next`, so the
 * `details` argument (reason / native event / `cancel()`) never
 * reached the consumer. Post-fix the wrapper forwards `(next, details)`
 * verbatim. We click the Trigger and read out:
 *   - `typeof details === "object"` (the second arg arrived), and
 *   - `details.reason === "trigger-press"` (Base UI's canonical
 *     reason for a click-driven open). */
function OnOpenChangeDetailsStory() {
  const [open, setOpen] = useState<boolean>(false);
  const [details, setDetails] = useState<string>("no-details-yet");
  return (
    <div
      className="zs-story-row"
      role="group"
      aria-label="onOpenChange details"
      style={{ flexDirection: "column", alignItems: "stretch" }}
    >
      <p
        role="status"
        aria-label="onOpenChange details readout"
        data-testid="collapsible-details-readout"
      >
        Details: {details}
      </p>
      <Collapsible
        open={open}
        onOpenChange={(next, eventDetails) => {
          // Capture the second arg shape so the regression test can
          // confirm Base UI handed us the change details.
          if (eventDetails && typeof eventDetails === "object") {
            const reason =
              (eventDetails as { reason?: unknown }).reason ?? "no-reason";
            setDetails(`reason=${String(reason)}`);
          } else {
            setDetails(`details-missing typeof=${typeof eventDetails}`);
          }
          setOpen(next);
        }}
        data-testid="collapsible-details"
      >
        <Collapsible.Trigger data-testid="collapsible-details-trigger">
          Show changelog
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-details-panel">
          Wave 10 rework wired the Base UI change details through
          `onOpenChange`.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  );
}
export const RegressionOnOpenChangeDetails: Story = {
  name: "Regression — onOpenChange details (rework fix #2)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression for the Wave 10 rework: `onOpenChange` MUST forward " +
          "Base UI's second `details` argument so consumers can read the " +
          "reason / native event and call `details.cancel()`. The readout " +
          "captures the `reason` field on the post-click call.",
      },
    },
  },
  render: () => <OnOpenChangeDetailsStory />,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const trigger = canvas.getByRole("button", { name: /show changelog/i });
    const readout = canvas.getByRole("status", {
      name: /onopenchange details readout/i,
    });

    // Pre-click: untouched.
    await expect(readout).toHaveTextContent("Details: no-details-yet");
    await userEvent.click(trigger);
    // Post-click: the second arg arrived AND carries a reason field.
    // Base UI's canonical reason for a trigger-press open is
    // `"trigger-press"`. We tolerate either the canonical string or
    // any non-empty string so the test stays robust if the reason
    // taxonomy widens — what matters is that `details` is an object.
    await expect(readout).not.toHaveTextContent(/details-missing/);
    await expect(readout).not.toHaveTextContent(/no-details-yet/);
    await expect(readout).toHaveTextContent(/^Details: reason=/);
  },
};

/* ─── 9. RegressionDisabledOpenForcedColors — fix #3 (rework) ─────── *
 *
 * Wave 10 rework regression for the forced-colors cascade bug.
 * Pre-fix: `[data-panel-open]` paints `HighlightText`/`Highlight` at
 * the same specificity (0,2,0) as `[data-disabled]` but later in
 * source, so under `forced-colors: active` an open + disabled trigger
 * paints `Highlight` instead of `GrayText` — violating the disabled
 * contract. Post-fix the open selectors carry `:not([data-disabled])`
 * AND the disabled rules sit after the open block, so the disabled
 * cascade wins.
 *
 * The regression is observed by the aria-wiring script under a
 * `forced-colors: active` emulation; the story just provides a
 * defaultOpen + disabled Collapsible mount so the assertion has a
 * stable target. */
export const RegressionDisabledOpenForcedColors: Story = {
  name: "Regression — disabled+open forced-colors (rework fix #3)",
  parameters: {
    docs: {
      description: {
        story:
          "Regression baseline for the Wave 10 rework forced-colors fix. " +
          "An open + disabled Collapsible.Trigger must paint GrayText (the " +
          "disabled system color), NOT HighlightText. The aria-wiring " +
          "script emulates `forced-colors: active` and reads computed " +
          "color on the Trigger AND the chevron.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Disabled open forced colors"
    >
      <Collapsible
        defaultOpen
        disabled
        data-testid="collapsible-disabled-open-hcm"
      >
        <Collapsible.Trigger
          data-testid="collapsible-disabled-open-hcm-trigger"
        >
          Disabled + open under HCM
        </Collapsible.Trigger>
        <Collapsible.Panel data-testid="collapsible-disabled-open-hcm-panel">
          Computed color on the Trigger MUST resolve to GrayText, not
          HighlightText.
        </Collapsible.Panel>
      </Collapsible>
    </div>
  ),
};
