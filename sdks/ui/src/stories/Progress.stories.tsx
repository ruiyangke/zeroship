import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
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
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", {
      name: /upload progress/i,
    });

    await expect(progress).toHaveAttribute("aria-valuemin", "0");
    await expect(progress).toHaveAttribute("aria-valuemax", "100");
    await expect(progress).toHaveAttribute("aria-valuenow", "42");
    await expect(progress).toHaveAttribute("data-status", "progressing");
  },
};

/* ─── 2. Indeterminate — value=null ────────────────────────────────── */
export const Indeterminate: Story = {
  name: "Indeterminate (loading)",
  parameters: {
    docs: {
      description: {
        story:
          "Pass `value={null}` for indeterminate mode. Base UI omits " +
          "`aria-valuenow`. To give AT users a meaningful readout " +
          "(Base UI's default is the literal string 'indeterminate " +
          "progress'), pass an explicit `aria-valuetext` that names " +
          "the activity. The indicator shimmers across the track (or, " +
          "under reduced motion, paints a static 100% dim fill).",
      },
    },
  },
  // Pass aria-valuetext explicitly because Base UI's default for an
  // indeterminate progress is the generic literal "indeterminate
  // progress" — overriding it with the activity name is the SR-friendly
  // read. (Pre-wave10 the story asserted `aria-valuetext === "Loading"`
  // but never forwarded the prop, so the assertion fell through to Base
  // UI's default and the play() failed in test-storybook.)
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Indeterminate progress">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={null}
          aria-label="Loading"
          aria-valuetext="Loading"
          data-testid="progress-indeterminate"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", { name: /loading/i });

    await expect(progress).not.toHaveAttribute("aria-valuenow");
    await expect(progress).toHaveAttribute("aria-valuetext", "Loading");
    await expect(progress).toHaveAttribute("data-status", "indeterminate");
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
  play: async ({ canvasElement }) => {
    // AllSizes renders three progressbars (sm / md / lg), all with
    // value=55. The play asserts every variant carries the determinate
    // value and that the labels expose the size via aria-label.
    const canvas = within(canvasElement);
    const bars = canvas.getAllByRole("progressbar");

    await expect(bars).toHaveLength(3);
    for (const bar of bars) {
      await expect(bar).toHaveAttribute("aria-valuenow", "55");
    }
    await expect(
      canvas.getByRole("progressbar", { name: /small progress/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("progressbar", { name: /medium progress/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("progressbar", { name: /large progress/i }),
    ).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    // WithValue renders a single bar with value=68 and showValue, so
    // both the aria-valuenow and the rendered "68%" string come from
    // the same Progress instance under the "Download progress" label.
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", {
      name: /download progress/i,
    });

    await expect(progress).toHaveAttribute("aria-valuenow", "68");
    await expect(canvas.getByText("68%")).toBeInTheDocument();
  },
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
  play: async ({ canvasElement }) => {
    // CompletionCelebrate renders value=100 with label="Backup"; Base UI
    // flips data-status to "complete" at the cap. The aria-label cascades
    // from the visible label since `label` is set but no aria-label override.
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", { name: /backup/i });

    await expect(progress).toHaveAttribute("aria-valuenow", "100");
    await expect(progress).toHaveAttribute("data-status", "complete");
    await expect(canvas.getByText("100%")).toBeInTheDocument();
  },
};

/* ─── 8. External aria labelling ───────────────────────────────────── */
export const ExternalAriaLabelling: Story = {
  name: "External aria labelling",
  parameters: {
    docs: {
      description: {
        story:
          "External aria-labelledby, aria-describedby, and aria-valuetext " +
          "forward to the progress root without clobbering determinate " +
          "value semantics. `showValue` is intentionally omitted on " +
          "custom-range progresses — Base UI's default percent formatter " +
          "divides the raw value by 100 (not by `max`), so a value=7/max=10 " +
          "bar would paint a misleading '7%' badge over a 70%-complete " +
          "track. The SR-only `aria-valuetext` carries the real readout; " +
          "the visible cue is the filled track itself.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Progress external aria">
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <span id="progress-import-label" className="zs-story-label">
          Import progress
        </span>
        <span id="progress-import-description" style={{ fontSize: "0.8125rem" }}>
          Rows copied into the workspace.
        </span>
        <Progress
          value={7}
          max={10}
          data-testid="progress-external-aria"
          aria-labelledby="progress-import-label"
          aria-describedby="progress-import-description"
          aria-valuetext="Seven of ten rows imported"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", {
      name: /import progress/i,
    });

    await expect(progress).toHaveAttribute("aria-valuenow", "7");
    await expect(progress).toHaveAttribute(
      "aria-valuetext",
      "Seven of ten rows imported",
    );
    await expect(progress).toHaveAttribute(
      "aria-describedby",
      "progress-import-description",
    );
    // Custom-range guardrail (mirror of Meter wave10 ExternalAria fix): no
    // misleading "N%" badge from Base UI's default `Intl.NumberFormat
    // {style: 'percent'}` (which divides by 100, not by `max`). The
    // determinate readout is the aria-valuetext above; sighted users get
    // the filled track. Both the raw-value and the would-be-correct-pct
    // strings must be absent from the rendered text.
    await expect(canvas.queryByText("7%")).not.toBeInTheDocument();
    await expect(canvas.queryByText("70%")).not.toBeInTheDocument();
  },
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

/* ─── 9. DisabledIndeterminate — Wave 10 fix #3 regression ───────────
 *
 * A `data-disabled + data-status="indeterminate"` row under
 * `prefers-reduced-motion: reduce` must keep its disabled paint
 * (`var(--zs-label-quaternary)` on the indicator, `var(--zs-fill-
 * quaternary)` on the track) — not the reduced-motion indeterminate
 * placeholder color-mix.
 *
 * Pre-fix the base `[data-disabled] .zs-progress__indicator` rule
 * lived above an equal-specificity rule inside `@media (prefers-
 * reduced-motion: reduce)` that re-painted the indicator to a dim
 * accent for indeterminate progress. Equal specificity + later source
 * order made the reduced-motion rule win, so disabled indeterminate
 * progress repainted as active accent under reduced motion.
 *
 * The aria-wiring runner (check-aria-wiring.mjs Wave 10 fix #3) emulates
 * `reducedMotion: 'reduce'`, opens this story, and asserts the indicator
 * computed background matches the disabled token, not the active fill.
 *
 * `data-disabled=""` is forwarded through the BaseProgress.Root rest
 * spread; the rendered Root carries both `data-disabled` and Base UI's
 * own `data-status="indeterminate"`. */
export const DisabledIndeterminate: Story = {
  name: "Disabled indeterminate",
  parameters: {
    docs: {
      description: {
        story:
          "Shows this component behavior with realistic content and keeps the edge case easy to inspect.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Disabled indeterminate"
    >
      <div className="zs-story-cell" style={{ inlineSize: "24rem" }}>
        <Progress
          value={null}
          aria-label="Paused upload"
          aria-valuetext="Paused"
          data-testid="progress-disabled-indeterminate"
          // data-* attrs forward through Base UI Progress.Root's rest
          // spread — the rendered Root carries both `data-disabled`
          // (consumer-set) and `data-status="indeterminate"` (Base UI).
          {...({ "data-disabled": "" } as Record<string, string>)}
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const progress = canvas.getByRole("progressbar", {
      name: /paused upload/i,
    });
    await expect(progress).toHaveAttribute("data-status", "indeterminate");
    await expect(progress).toHaveAttribute("data-disabled", "");
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
