import type { Meta, StoryObj } from "@storybook/react";
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

/* ─── 3. With label (Field integration) ────────────────────────────── */
export const WithLabel: Story = {
  name: "With label",
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Switch with label">
      <div className="zs-story-cell" style={{ maxWidth: "22rem" }}>
        <Field>
          <Field.Label>Notifications</Field.Label>
          <Switch data-testid="switch-with-label" name="notifications" />
        </Field>
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
