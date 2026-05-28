import type { Meta, StoryObj } from "@storybook/react";
import { Meter, meterStatus } from "../components";

const meta: Meta<typeof Meter> = {
  title: "Components/Meter",
  component: Meter,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Meter>;

/* ─── 1. Basic — 60% ───────────────────────────────────────────────── */
export const Basic: Story = {
  name: "Basic (60%)",
  parameters: {
    docs: {
      description: {
        story:
          "Default neutral-intent meter at 60%. Reads as `role=meter`, " +
          "`aria-valuemin=0`, `aria-valuemax=100`, `aria-valuenow=60`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic meter">
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <Meter
          value={60}
          aria-label="Disk usage"
          data-testid="meter-basic"
        />
      </div>
    </div>
  ),
};

/* ─── 2. AllIntents — 4 colors at the same value ──────────────────── */
export const AllIntents: Story = {
  name: "All intents",
  parameters: {
    docs: {
      description: {
        story:
          "Four intents at the same 50% value. Neutral = accent; " +
          "success = green; warning = amber-orange; danger = red. The " +
          "track stays neutral fill across all intents.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All intents"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Neutral</span>
        <Meter
          value={50}
          intent="neutral"
          aria-label="Neutral meter"
          data-testid="meter-neutral"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Success</span>
        <Meter
          value={50}
          intent="success"
          aria-label="Success meter"
          data-testid="meter-success"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Warning</span>
        <Meter
          value={50}
          intent="warning"
          aria-label="Warning meter"
          data-testid="meter-warning"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Danger</span>
        <Meter
          value={50}
          intent="danger"
          aria-label="Danger meter"
          data-testid="meter-danger"
        />
      </div>
    </div>
  ),
};

/* ─── 3. AllSizes — sm / md / lg ───────────────────────────────────── */
export const AllSizes: Story = {
  name: "All sizes",
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="All sizes"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Small (track 0.25rem)</span>
        <Meter
          size="sm"
          value={70}
          aria-label="Small meter"
          data-testid="meter-sm"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Medium (track 0.375rem)</span>
        <Meter
          size="md"
          value={70}
          aria-label="Medium meter"
          data-testid="meter-md"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "20rem" }}>
        <span className="zs-story-label">Large (track 0.5rem)</span>
        <Meter
          size="lg"
          value={70}
          aria-label="Large meter"
          data-testid="meter-lg"
        />
      </div>
    </div>
  ),
};

/* ─── 4. WithValue — label + percent ───────────────────────────────── */
export const WithValue: Story = {
  name: "With value (label + percent)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `label` + `showValue` to get a header row with the " +
          "label at the inline-start and the formatted percentage at " +
          "the inline-end. Label auto-associates with the meter via " +
          "Base UI's aria-labelledby chain.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With value">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Meter
          value={72}
          label="Storage used"
          showValue
          data-testid="meter-with-value"
        />
      </div>
    </div>
  ),
};

/* ─── 5. Ranges — heuristic-driven intent at three values ──────────── */
export const Ranges: Story = {
  name: "Ranges (heuristic intent)",
  parameters: {
    docs: {
      description: {
        story:
          "Three meters at 15 / 50 / 85% — the `meterStatus(value)` " +
          "helper picks danger (<25%) / warning (25-75%) / success " +
          "(>75%) so the intent telegraphs the threshold bucket at a " +
          "glance. Each row uses the helper at the call site.",
      },
    },
  },
  render: () => {
    const values = [15, 50, 85];
    return (
      <div
        className="zs-story-row"
        role="group"
        aria-label="Ranges"
        style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
      >
        {values.map((v) => (
          <div
            key={v}
            className="zs-story-cell"
            style={{ inlineSize: "24rem" }}
          >
            <Meter
              value={v}
              intent={meterStatus(v)}
              label={`Battery (${meterStatus(v)})`}
              showValue
              data-testid={`meter-range-${v}`}
            />
          </div>
        ))}
      </div>
    );
  },
};

/* ─── 6. Disabled ──────────────────────────────────────────────────── */
export const Disabled: Story = {
  name: "Disabled",
  parameters: {
    docs: {
      description: {
        story:
          "When the row is `data-disabled` (consumer-supplied) the " +
          "track dims and the indicator collapses to the label-" +
          "quaternary token — reads as 'measurement unavailable'.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Disabled">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Meter
          value={42}
          label="Signal"
          showValue
          aria-disabled="true"
          data-disabled=""
          data-testid="meter-disabled"
        />
      </div>
    </div>
  ),
};

/* ─── 7. RTL ──────────────────────────────────────────────────────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Right-to-left writing-mode. The indicator fills from the " +
          "right (logical inline-start in RTL); the header row swaps " +
          "label and value positions automatically.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL meter">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Meter
          value={67}
          label="مساحة التخزين"
          showValue
          data-testid="meter-rtl"
        />
      </div>
    </div>
  ),
};
