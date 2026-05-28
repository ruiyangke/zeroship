import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { useState } from "react";
import { Button, Field, Switch } from "../components";

const meta: Meta<typeof Switch> = {
  title: "Components/Switch",
  component: Switch,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Switch>;

/* ─── 1. All states ────────────────────────────────────────────────── */
export const AllStates: Story = {
  name: "All states",
  parameters: {
    docs: {
      description: {
        story:
          "Five visual states — off / on / disabled-off / disabled-on / " +
          "readonly. The thumb position carries the on/off signal so a " +
          "colorblind user reads the same state a sighted user does.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All switch states">
      <div className="zs-story-cell">
        <span className="zs-story-label">Off</span>
        <Switch label="Off" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">On</span>
        <Switch label="On" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — off</span>
        <Switch label="Disabled" disabled />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Disabled — on</span>
        <Switch label="Disabled, on" disabled defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Read only</span>
        <Switch label="Read only" readOnly defaultChecked />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const off = canvas.getByRole("switch", { name: /^off$/i });
    const on = canvas.getByRole("switch", { name: /^on$/i });
    const disabled = canvas.getByRole("switch", { name: /^disabled$/i });
    const readOnly = canvas.getByRole("switch", { name: /^read only$/i });

    await expect(off).toHaveAttribute("aria-checked", "false");
    await userEvent.click(off);
    await waitFor(() => expect(off).toHaveAttribute("aria-checked", "true"));

    await expect(on).toHaveAttribute("aria-checked", "true");
    await userEvent.click(on);
    await waitFor(() => expect(on).toHaveAttribute("aria-checked", "false"));

    await expect(disabled).toHaveAttribute("data-disabled");
    await userEvent.click(disabled);
    await expect(disabled).toHaveAttribute("aria-checked", "false");

    await expect(readOnly).toHaveAttribute("aria-checked", "true");
    await userEvent.click(readOnly);
    await expect(readOnly).toHaveAttribute("aria-checked", "true");
  },
};

/* ─── 2. All sizes ─────────────────────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="All switch sizes">
      <div className="zs-story-cell">
        <span className="zs-story-label">Small</span>
        <Switch size="sm" label="Small" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Medium</span>
        <Switch size="md" label="Medium" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Large</span>
        <Switch size="lg" label="Large" defaultChecked />
      </div>
    </div>
  ),
};

/* ─── 3. With external label (Field integration) ─────────────────────
 *
 * Renamed from `WithLabel` (slice-4 visual-polish item 6). Composing
 * Field.Label ABOVE the switch is one valid layout; the inline
 * `label` prop pattern (see the `Inline` story below) is the
 * visually-recommended default for a single boolean. Naming this
 * story `WithExternalLabel` makes the choice between the two patterns
 * self-describing. The aria-wiring assertion (`Switch bare track
 * focus ring`) and the capture-script entry both follow this name.
 *
 * Note: the aria-wiring suite does NOT have a `WithLabel — label
 * click toggles` assertion for Switch (only Checkbox has that one),
 * so the rename here is story-local and doesn't require updating any
 * existing assertion. */
export const WithExternalLabel: Story = {
  name: "With external label",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Switch with external label">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>Notifications</Field.Label>
          <Switch data-testid="switch-with-label" name="notifications" />
        </Field>
      </div>
    </div>
  ),
};

/* ─── 3b. Inline label (recommended default for single booleans) ─────
 *
 * Slice-4 visual-polish item 6. Codex flagged the lone-switch-under-
 * a-Field.Label pattern as visually orphaned. The Switch component's
 * inline `label` prop wraps track + text into one click surface via
 * SelectionRow — this is the right visual default for a settings-
 * style toggle. */
export const Inline: Story = {
  name: "Inline label (recommended default)",
  parameters: {
    docs: {
      description: {
        story:
          "Single-boolean default: the inline `label` prop wraps the " +
          "track + label text into one `<label>` so the whole row is the " +
          "click surface. Use this for settings rows; reach for " +
          "`WithExternalLabel` only when you need a Field.Description " +
          "or another block-level element above the switch.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Switch inline label">
      <div className="zs-story-cell">
        <Switch label="Notifications" name="notifications-inline" defaultChecked />
      </div>
      <div className="zs-story-cell">
        <Switch label="Public profile" name="public-inline" />
      </div>
    </div>
  ),
};

/* ─── 4. With description ──────────────────────────────────────────── */
export const WithDescription: Story = {
  name: "With description",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Switch with description">
      <div className="zs-story-cell" style={{ maxWidth: "24rem" }}>
        <Field>
          <Field.Label>Auto-renew</Field.Label>
          <Switch name="auto-renew" defaultChecked />
          <Field.Description>
            Your plan renews on the 1st of every month. Turn off to receive an
            invoice for manual renewal.
          </Field.Description>
        </Field>
      </div>
    </div>
  ),
};

/* ─── 5. Immediate effect (settings-style) ─────────────────────────── */
export const ImmediateEffect: Story = {
  name: "Immediate effect",
  parameters: {
    docs: {
      description: {
        story:
          "Switches are settings-style: flipping the switch must change " +
          "something on the same frame the user releases. The status line " +
          "below mirrors the switch state with no debounce — verified by " +
          "the aria-wiring assertion that asserts the text changes within " +
          "100ms of the click.",
      },
    },
  },
  render: function ImmediateEffectRender() {
    const [on, setOn] = useState(false);
    return (
      <div className="zs-story-row" role="group" aria-label="Switch immediate effect">
        <div
          className="zs-story-cell"
          style={{ maxWidth: "24rem", display: "flex", flexDirection: "column", gap: "0.5rem" }}
        >
          <Switch
            data-testid="switch-immediate"
            label="Dark mode"
            checked={on}
            onCheckedChange={setOn}
          />
          <span
            data-testid="switch-immediate-status"
            style={{ fontSize: "0.8125rem" }}
          >
            status: {on ? "on" : "off"}
          </span>
        </div>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const darkMode = canvas.getByRole("switch", { name: /dark mode/i });
    const status = canvas.getByText(/^status:/i);

    await expect(status).toHaveTextContent("status: off");
    await userEvent.click(darkMode);
    await waitFor(() => expect(status).toHaveTextContent("status: on"));
    await expect(darkMode).toHaveAttribute("aria-checked", "true");
  },
};

/* ─── 6. Inside a form ─────────────────────────────────────────────── */
export const InsideForm: Story = {
  name: "Inside a form",
  render: function InsideFormRender() {
    const [submitted, setSubmitted] = useState<string>("(not submitted)");
    return (
      <div className="zs-story-row" role="group" aria-label="Switch inside form">
        <form
          className="zs-story-cell"
          style={{ maxWidth: "24rem", display: "flex", flexDirection: "column", gap: "0.75rem" }}
          onSubmit={(e) => {
            e.preventDefault();
            const data = new FormData(e.currentTarget);
            const entries = Array.from(data.entries())
              .map(([k, v]) => `${k}=${String(v)}`)
              .join(", ");
            setSubmitted(entries.length === 0 ? "(empty)" : entries);
          }}
        >
          <Switch label="Receive newsletters" name="newsletters" defaultChecked />
          <Switch label="Public profile" name="public" />
          <Button type="submit" variant="filled">
            Save preferences
          </Button>
          <span style={{ fontSize: "0.8125rem" }} data-testid="switch-form-result">
            submitted: {submitted}
          </span>
        </form>
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const newsletters = canvas.getByRole("switch", {
      name: /receive newsletters/i,
    });
    const publicProfile = canvas.getByRole("switch", { name: /public profile/i });
    const save = canvas.getByRole("button", { name: /save preferences/i });
    const result = canvas.getByText(/^submitted:/i);

    await expect(newsletters).toHaveAttribute("aria-checked", "true");
    await userEvent.click(save);
    await waitFor(() => expect(result).toHaveTextContent("newsletters=on"));

    await userEvent.click(publicProfile);
    await userEvent.click(save);
    await waitFor(() => expect(result).toHaveTextContent("public=on"));
  },
};

/* ─── 7. Disabled (Field cascade) ──────────────────────────────────── */
export const Disabled: Story = {
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Switch disabled cascade">
      <div className="zs-story-cell">
        <span className="zs-story-label">disabled prop</span>
        <Switch label="Locked off" disabled />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">Field disabled — switch inherits</span>
        <Field disabled>
          <Field.Label>Billing alerts</Field.Label>
          <Switch name="billing-alerts" defaultChecked />
          <Field.Description>Add a card to enable.</Field.Description>
        </Field>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const locked = canvas.getByRole("switch", { name: /locked off/i });
    const inherited = canvas.getByRole("switch", { name: /billing alerts/i });

    await expect(locked).toHaveAttribute("data-disabled");
    await expect(inherited).toHaveAttribute("data-disabled");
    await userEvent.click(locked);
    await userEvent.click(inherited);
    await expect(locked).toHaveAttribute("aria-checked", "false");
    await expect(inherited).toHaveAttribute("aria-checked", "true");
  },
};

/* ─── 8. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew label. With `dir=\"rtl\"`, the thumb still slides toward " +
          "the inline-end edge (visually LEFT in RTL) when checked — the " +
          "CSS keys two direction-conditional translate rules off " +
          "`[dir=\"rtl\"]` so the on-state position stays semantically " +
          "correct.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL switch row">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>התראות</Field.Label>
          <Switch defaultChecked name="notifications-rtl" />
        </Field>
      </div>
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>מצב כהה</Field.Label>
          <Switch name="darkmode-rtl" />
        </Field>
      </div>
    </div>
  ),
};
