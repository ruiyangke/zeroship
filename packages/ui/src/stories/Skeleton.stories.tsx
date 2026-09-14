import type { Meta, StoryObj } from "@storybook/react";
import { expect, within } from "@storybook/test";
import { Skeleton } from "../components";

const meta: Meta<typeof Skeleton> = {
  title: "Components/Skeleton",
  component: Skeleton,
  parameters: { layout: "fullscreen" },
};

export default meta;

type Story = StoryObj<typeof Skeleton>;

/* ─── 1. Variants — text / rect / circle ────────────────────────────── */
export const Variants: Story = {
  name: "Variants (text / rect / circle)",
  parameters: {
    docs: {
      description: {
        story:
          "The three placeholder shapes. Each is `aria-hidden` (no role) " +
          "— the page's status/Spinner conveys loading, not the skeleton.",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Skeleton variants"
      style={{ flexDirection: "column", alignItems: "stretch", gap: "1.5rem" }}
    >
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <span className="zs-story-label">text</span>
        <Skeleton variant="text" data-testid="skeleton-text" />
      </div>
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <span className="zs-story-label">rect</span>
        <Skeleton variant="rect" height="6rem" data-testid="skeleton-rect" />
      </div>
      <div className="zs-story-cell">
        <span className="zs-story-label">circle</span>
        <Skeleton
          variant="circle"
          width="3rem"
          height="3rem"
          data-testid="skeleton-circle"
        />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    for (const id of ["skeleton-text", "skeleton-rect", "skeleton-circle"]) {
      const el = canvas.getByTestId(id);
      // Decorative: aria-hidden, no role.
      await expect(el).toHaveAttribute("aria-hidden", "true");
    }
    await expect(canvas.getByTestId("skeleton-circle")).toHaveAttribute(
      "data-variant",
      "circle",
    );
  },
};

/* ─── 2. Multi-line text ────────────────────────────────────────────── */
export const MultiLineText: Story = {
  name: "Multi-line text (paragraph)",
  parameters: {
    docs: {
      description: {
        story:
          "`variant='text'` with `lines > 1` renders N stacked bars; the " +
          "last bar is shortened so the block reads as a paragraph.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Multi-line skeleton">
      <div className="zs-story-cell" style={{ inlineSize: "min(24rem, 100%)" }}>
        <Skeleton variant="text" lines={4} data-testid="skeleton-lines" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const container = canvas.getByTestId("skeleton-lines");
    await expect(container).toHaveAttribute("data-lines", "4");
    await expect(container).toHaveAttribute("aria-hidden", "true");
    // Four bars rendered.
    const bars = container.querySelectorAll('[data-slot="skeleton-line"]');
    await expect(bars).toHaveLength(4);
  },
};

/* ─── 2b. Circle stays round with a single dimension (regression guard) ─ *
 *
 * A circle skeleton given only `width` must stay round — `aspect-ratio: 1`
 * derives the other axis. Pre-fix the variant locked BOTH inline+block to
 * the default size, so a lone `width` produced an ellipse. */
export const CircleSingleDimension: Story = {
  name: "Circle stays round (single dimension)",
  parameters: {
    docs: {
      description: {
        story:
          "A circle given only `width` keeps a 1:1 aspect via " +
          "`aspect-ratio: 1` (block-size derives from inline-size) — no " +
          "ellipse when a single dimension is passed.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Circle single dimension">
      <div className="zs-story-cell">
        <Skeleton variant="circle" width="4rem" data-testid="skeleton-circle-w" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const el = canvas.getByTestId("skeleton-circle-w") as HTMLElement;
    // Round: rendered width === rendered height despite only `width` set.
    await expect(el.offsetWidth).toBe(el.offsetHeight);
    await expect(getComputedStyle(el).aspectRatio).toBe("1 / 1");
  },
};

/* ─── 3. Reduced motion ─────────────────────────────────────────────── *
 *
 * Under prefers-reduced-motion the shimmer animation is disabled while
 * the static base fill stays painted (paint preserved). The CSS gates
 * `animation: none` + `background-image: none` inside the
 * `@media (prefers-reduced-motion: reduce)` block; the base
 * `background-color: var(--zs-fill-secondary)` remains. This story
 * documents that contract and asserts the placeholder still renders
 * (visible) regardless of the motion preference. */
export const ReducedMotion: Story = {
  name: "Reduced motion (animation disabled, paint preserved)",
  parameters: {
    docs: {
      description: {
        story:
          "Under `prefers-reduced-motion: reduce` the shimmer stops " +
          "(`animation: none`) but the static fill remains painted, so " +
          "the placeholder is still visible. The motion gate lives in " +
          "Skeleton.css; this story asserts the placeholder renders.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Reduced-motion skeleton">
      <div className="zs-story-cell" style={{ inlineSize: "min(20rem, 100%)" }}>
        <Skeleton variant="rect" height="4rem" data-testid="skeleton-rm" />
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const el = canvasElement.querySelector<HTMLElement>(
      '[data-testid="skeleton-rm"]',
    );
    await expect(el).not.toBeNull();
    if (el == null) return;
    // Paint preserved: the base fill is always set (never transparent),
    // so the placeholder is visible with or without motion.
    const bg = getComputedStyle(el).backgroundColor;
    await expect(bg).not.toBe("rgba(0, 0, 0, 0)");
    await expect(bg).not.toBe("transparent");
  },
};
