import type { Meta, StoryObj } from "@storybook/react";
import { Progress } from "../components";

const meta: Meta<typeof Progress> = {
  title: "Components/Progress",
  component: Progress,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof Progress>;

/* ─── 1. Determinate — 42% ─────────────────────────────────────────── */
export const Determinate: Story = {
  name: "Determinate (42%)",
  parameters: {
    docs: {
      description: {
        story:
          "Stock determinate progress bar. `role=progressbar`, " +
          "`aria-valuemin=0`, `aria-valuemax=100`, `aria-valuenow=42`.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Determinate progress">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={42}
          aria-label="Upload progress"
          data-testid="progress-determinate"
        />
      </div>
    </div>
  ),
};

/* ─── 2. Indeterminate — value=null ────────────────────────────────── */
export const Indeterminate: Story = {
  name: "Indeterminate (loading)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `value={null}` for indeterminate mode. Base UI omits " +
          "`aria-valuenow` and adds `aria-valuetext='Loading'`; the " +
          "indicator shimmers across the track (or, under reduced " +
          "motion, paints a static 100% dim fill).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Indeterminate progress">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={null}
          aria-label="Loading"
          data-testid="progress-indeterminate"
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
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <span className="zs-story-label">Small (track 0.25rem)</span>
        <Progress
          size="sm"
          value={55}
          aria-label="Small progress"
          data-testid="progress-sm"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <span className="zs-story-label">Medium (track 0.375rem)</span>
        <Progress
          size="md"
          value={55}
          aria-label="Medium progress"
          data-testid="progress-md"
        />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <span className="zs-story-label">Large (track 0.5rem)</span>
        <Progress
          size="lg"
          value={55}
          aria-label="Large progress"
          data-testid="progress-lg"
        />
      </div>
    </div>
  ),
};

/* ─── 4. WithValue — percentage label ──────────────────────────────── */
export const WithValue: Story = {
  name: "With value (percent label)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `showValue` to render the formatted percentage at the " +
          "inline-end of the header row. The label-less case puts the " +
          "value flush right; combine with `label` for the canonical " +
          "Progress-with-context shape (see WithLabel).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With value">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={68}
          showValue
          aria-label="Download progress"
          data-testid="progress-with-value"
        />
      </div>
    </div>
  ),
};

/* ─── 5. CompletionCelebrate — value === max ───────────────────────── */
export const CompletionCelebrate: Story = {
  name: "Completion (100%)",
  parameters: {
    docs: {
      description: {
        story:
          "At 100% Base UI emits `data-status='complete'` and the bar " +
          "transitions from accent to system-green so the eye reads " +
          "'done' instead of 'still in progress'.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Completion">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={100}
          label="Backup"
          showValue
          data-testid="progress-complete"
        />
      </div>
    </div>
  ),
};

/* ─── 6. WithLabel — label + showValue ─────────────────────────────── */
export const WithLabel: Story = {
  name: "With label",
  parameters: {
    docs: {
      description: {
        story:
          "Label sits at the inline-start of the header; value at the " +
          "inline-end. Label auto-associates with the progress via " +
          "Base UI's labelledby chain.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With label">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={33}
          label="Uploading photo.jpg"
          showValue
          data-testid="progress-with-label"
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
          "right; the indeterminate shimmer (when present) sweeps " +
          "right-to-left.",
      },
    },
  },
  render: () => (
    <div dir="rtl" className="zs-story-row" role="group" aria-label="RTL progress">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={42}
          label="جاري التحميل"
          showValue
          data-testid="progress-rtl"
        />
      </div>
    </div>
  ),
};
