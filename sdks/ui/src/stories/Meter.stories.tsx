import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const meter = canvas.getByRole("meter", { name: /disk usage/i });

    await expect(meter).toHaveAttribute("aria-valuemin", "0");
    await expect(meter).toHaveAttribute("aria-valuemax", "100");
    await expect(meter).toHaveAttribute("aria-valuenow", "60");
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const intent of ["neutral", "success", "warning", "danger"]) {
      await expect(
        canvas.getByRole("meter", { name: new RegExp(`${intent} meter`, "i") }),
      ).toHaveAttribute("data-intent", intent);
    }
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    // AllSizes renders three rows (Small / Medium / Large) at value=70.
    // Wave-9 fix: the previous play asserted /storage used/i + 72% which
    // belonged to the WithValue story; against AllSizes the lookup would
    // throw immediately. Real path is three meters at aria-valuenow=70.
    for (const label of [/small meter/i, /medium meter/i, /large meter/i]) {
      const meter = canvas.getByRole("meter", { name: label });
      await expect(meter).toHaveAttribute("aria-valuenow", "70");
    }
    await expect(
      canvas.getByRole("meter", { name: /small meter/i }),
    ).toHaveAttribute("data-size", "sm");
    await expect(
      canvas.getByRole("meter", { name: /medium meter/i }),
    ).toHaveAttribute("data-size", "md");
    await expect(
      canvas.getByRole("meter", { name: /large meter/i }),
    ).toHaveAttribute("data-size", "lg");
  },
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    await expect(meterStatus(15)).toBe("danger");
    await expect(meterStatus(50)).toBe("warning");
    await expect(meterStatus(85)).toBe("success");
    await expect(meterStatus(10, 10, 10)).toBe("neutral");
    await expect(
      canvas.getByRole("meter", { name: /battery \(danger\)/i }),
    ).toHaveAttribute("data-intent", "danger");
    await expect(
      canvas.getByRole("meter", { name: /battery \(warning\)/i }),
    ).toHaveAttribute("data-intent", "warning");
    await expect(
      canvas.getByRole("meter", { name: /battery \(success\)/i }),
    ).toHaveAttribute("data-intent", "success");
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const meter = canvas.getByRole("meter", { name: /signal/i });

    await expect(meter).toHaveAttribute("aria-valuenow", "42");
    await expect(meter).toHaveAttribute("aria-disabled", "true");
  },
};

/* ─── 8. External aria labelling ───────────────────────────────────── */
export const ExternalAriaLabelling: Story = {
  name: "External aria labelling",
  parameters: {
    docs: {
      description: {
        story:
          "External aria-labelledby and aria-describedby forwarded " +
          "directly to the meter root. Custom range (0-5) carries an " +
          "explicit aria-valuetext (\"Three of five incidents\") so " +
          "assistive tech reads the measurement in domain units. The " +
          "showValue badge is intentionally NOT used here — Base UI's " +
          "default formatter normalizes value/max to a percent string, " +
          "which on a 3/5 meter would render the misleading text " +
          "\"3%\". Custom-range meters should rely on aria-valuetext " +
          "for the screen-reader readout and let the bar telegraph the " +
          "ratio visually.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Meter external aria">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <span id="meter-incidents-label" className="zs-story-label">
          Incident budget
        </span>
        <span id="meter-incidents-description" style={{ fontSize: "0.8125rem" }}>
          Current incidents out of the weekly cap.
        </span>
        <Meter
          value={3}
          min={0}
          max={5}
          aria-labelledby="meter-incidents-label"
          aria-describedby="meter-incidents-description"
          aria-valuetext="Three of five incidents"
          data-testid="meter-external-aria"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const meter = canvas.getByRole("meter", { name: /incident budget/i });

    await expect(meter).toHaveAttribute("aria-valuenow", "3");
    await expect(meter).toHaveAttribute("aria-valuemin", "0");
    await expect(meter).toHaveAttribute("aria-valuemax", "5");
    await expect(meter).toHaveAttribute(
      "aria-valuetext",
      "Three of five incidents",
    );
    await expect(meter).toHaveAttribute(
      "aria-describedby",
      "meter-incidents-description",
    );
    // Wave-9 regression for the misleading-percent 🔴: with showValue
    // dropped, Base UI's default percent formatter is NOT mounted, so
    // the row must not surface a "3%" badge against the 0..5 range.
    await expect(canvas.queryByText("3%")).not.toBeInTheDocument();
    await expect(canvas.queryByText("60%")).not.toBeInTheDocument();
  },
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
