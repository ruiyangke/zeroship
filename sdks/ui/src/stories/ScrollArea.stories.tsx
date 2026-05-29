import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { Card, ScrollArea } from "../components";

const meta: Meta<typeof ScrollArea> = {
  title: "Components/ScrollArea",
  component: ScrollArea,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof ScrollArea>;

/* ─── Story content helpers ────────────────────────────────────────── *
 *
 * Self-contained so PNG captures are reproducible — no external image
 * fetches, no remote data. Long lists are generated inline; the grid
 * story paints a fixed-size matrix that overflows both axes. */

function makeListItems(count: number): string[] {
  const items: string[] = [];
  for (let i = 0; i < count; i++) {
    items.push(`List item ${String(i + 1).padStart(3, "0")}`);
  }
  return items;
}

/* ─── 1. BasicVertical — default `type="auto"`, vertical overflow ──── */
export const BasicVertical: Story = {
  name: "Basic vertical",
  parameters: {
    docs: {
      description: {
        story:
          "Vertical scroll over auto-fade chrome. The bar appears " +
          "while scrolling and fades after `scrollHideDelay` (600ms " +
          "default). Native keyboard scroll (ArrowDown, PageDown, End) " +
          "works on the focused Viewport.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="BasicVertical">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "12rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea data-testid="scrollarea-basic-vertical">
          <div style={{ padding: "var(--zs-space-4)" }}>
            {makeListItems(40).map((label) => (
              <div
                key={label}
                style={{
                  paddingBlock: "var(--zs-space-2)",
                  borderBlockEnd: "0.0625rem solid var(--zs-separator)",
                }}
              >
                {label}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-basic-vertical");
    await expect(root).toBeInTheDocument();
    // Base UI mounts the Scrollbar element once its overflow observer
    // fires — that can lag a frame behind initial paint. Poll instead
    // of asserting eagerly.
    await waitFor(() => {
      const verticalBar = root.querySelector(
        '[data-orientation="vertical"].zs-scrollarea__scrollbar',
      );
      expect(verticalBar).not.toBeNull();
    });
  },
};

/* ─── 2. BasicHorizontal — horizontal overflow only ────────────────── */
export const BasicHorizontal: Story = {
  name: "Basic horizontal",
  parameters: {
    docs: {
      description: {
        story:
          "Horizontal-only scroll for a wide image strip / table. " +
          "The shorthand emits a single horizontal Scrollbar pinned to " +
          "the block-end edge.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="BasicHorizontal">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "6rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea
          orientation="horizontal"
          data-testid="scrollarea-basic-horizontal"
        >
          <div
            style={{
              display: "flex",
              gap: "var(--zs-space-3)",
              padding: "var(--zs-space-4)",
              inlineSize: "max-content",
            }}
          >
            {makeListItems(20).map((label) => (
              <div
                key={label}
                style={{
                  inlineSize: "6rem",
                  blockSize: "3rem",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  borderRadius: "var(--zs-radius-3)",
                  background: "var(--zs-fill-tertiary)",
                  flexShrink: 0,
                }}
              >
                {label}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-basic-horizontal");
    await waitFor(() => {
      const horizontalBar = root.querySelector(
        '[data-orientation="horizontal"].zs-scrollarea__scrollbar',
      );
      expect(horizontalBar).not.toBeNull();
    });
    // No vertical bar in this shorthand — the orientation prop is
    // horizontal-only, so the shorthand doesn't render a vertical
    // Scrollbar at all.
    const verticalBar = root.querySelector(
      '[data-orientation="vertical"].zs-scrollarea__scrollbar',
    );
    await expect(verticalBar).toBeNull();
  },
};

/* ─── 3. Both — overflow on both axes + Corner ─────────────────────── */
export const Both: Story = {
  name: "Both axes",
  parameters: {
    docs: {
      description: {
        story:
          "Both vertical and horizontal scrollbars plus the corner " +
          "square at their intersection. Use for wide tables or any " +
          "two-axis overflow surface.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Both">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "12rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea orientation="both" data-testid="scrollarea-both">
          <div
            style={{
              padding: "var(--zs-space-4)",
              inlineSize: "max-content",
            }}
          >
            {makeListItems(30).map((label, rowIndex) => (
              <div
                key={label}
                style={{
                  display: "flex",
                  gap: "var(--zs-space-3)",
                  marginBlockEnd: "var(--zs-space-2)",
                }}
              >
                {[0, 1, 2, 3, 4, 5].map((col) => (
                  <div
                    key={col}
                    style={{
                      inlineSize: "6rem",
                      flexShrink: 0,
                      paddingInline: "var(--zs-space-3)",
                      paddingBlock: "var(--zs-space-2)",
                      borderRadius: "var(--zs-radius-2)",
                      background:
                        (rowIndex + col) % 2 === 0
                          ? "var(--zs-fill-tertiary)"
                          : "var(--zs-fill-quaternary)",
                    }}
                  >
                    {label}.{col}
                  </div>
                ))}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-both");
    await waitFor(() => {
      const vertical = root.querySelector(
        '[data-orientation="vertical"].zs-scrollarea__scrollbar',
      );
      const horizontal = root.querySelector(
        '[data-orientation="horizontal"].zs-scrollarea__scrollbar',
      );
      const corner = root.querySelector(".zs-scrollarea__corner");
      expect(vertical).not.toBeNull();
      expect(horizontal).not.toBeNull();
      expect(corner).not.toBeNull();
    });
  },
};

/* ─── 4. AlwaysVisible — `type="always"` ───────────────────────────── */
export const AlwaysVisible: Story = {
  name: "Always visible",
  parameters: {
    docs: {
      description: {
        story:
          "`type=\"always\"` pins the scrollbar at opacity 1 regardless " +
          "of scroll/hover state. Use when consumers benefit from a " +
          "persistent scroll cue (e.g. dashboards, lists with progress " +
          "indication).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AlwaysVisible">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "12rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea
          type="always"
          data-testid="scrollarea-always-visible"
        >
          <div style={{ padding: "var(--zs-space-4)" }}>
            {makeListItems(30).map((label) => (
              <div
                key={label}
                style={{
                  paddingBlock: "var(--zs-space-2)",
                  borderBlockEnd: "0.0625rem solid var(--zs-separator)",
                }}
              >
                {label}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-always-visible");
    await expect(root).toHaveAttribute("data-visibility", "always");
  },
};

/* ─── 5. HoverOnly — `type="hover"` ────────────────────────────────── */
export const HoverOnly: Story = {
  name: "Hover only",
  parameters: {
    docs: {
      description: {
        story:
          "`type=\"hover\"` keeps the scrollbar hidden until the pointer " +
          "enters the Viewport. Useful for content-dense surfaces where " +
          "chrome shouldn't distract until the user reaches for it.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="HoverOnly">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "12rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea
          type="hover"
          data-testid="scrollarea-hover-only"
        >
          <div style={{ padding: "var(--zs-space-4)" }}>
            {makeListItems(30).map((label) => (
              <div
                key={label}
                style={{
                  paddingBlock: "var(--zs-space-2)",
                  borderBlockEnd: "0.0625rem solid var(--zs-separator)",
                }}
              >
                {label}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-hover-only");
    await expect(root).toHaveAttribute("data-visibility", "hover");
    // `type="hover"` keeps the Scrollbar mounted regardless of overflow
    // (the shorthand passes `keepMounted` for the always/hover policies)
    // so the bar element is in the DOM at resting opacity 0.
    await waitFor(() => {
      const bar = root.querySelector(
        '[data-orientation="vertical"].zs-scrollarea__scrollbar',
      );
      expect(bar).not.toBeNull();
    });
  },
};

/* ─── 6. LongList — 200 items, stress-test the thumb sizing ────────── */
export const LongList: Story = {
  name: "Long list (200 items)",
  parameters: {
    docs: {
      description: {
        story:
          "200 items in a constrained Viewport — exercises the thumb's " +
          "minimum-size enforcement (Base UI sizes the thumb " +
          "proportionally; we floor it at `--zs-space-6` so the handle " +
          "stays grabbable on long content).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="LongList">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "16rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea data-testid="scrollarea-long-list">
          <div style={{ padding: "var(--zs-space-4)" }}>
            {makeListItems(200).map((label) => (
              <div
                key={label}
                style={{
                  paddingBlock: "var(--zs-space-2)",
                  borderBlockEnd: "0.0625rem solid var(--zs-separator)",
                }}
              >
                {label}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-long-list");
    await waitFor(() => {
      const thumb = root.querySelector(".zs-scrollarea__thumb");
      expect(thumb).not.toBeNull();
    });
  },
};

/* ─── 7. GridContent — horizontal + vertical overflow over a grid ──── */
export const GridContent: Story = {
  name: "Grid content",
  parameters: {
    docs: {
      description: {
        story:
          "A CSS Grid overflowing in both directions. Demonstrates the " +
          "two-axis Scrollbar + Corner anatomy carrying real layout, " +
          "not just text rows.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="GridContent">
      <div
        style={{
          inlineSize: "22rem",
          blockSize: "14rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea
          orientation="both"
          data-testid="scrollarea-grid-content"
        >
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(10, 6rem)",
              gridAutoRows: "4rem",
              gap: "var(--zs-space-2)",
              padding: "var(--zs-space-4)",
              inlineSize: "max-content",
            }}
          >
            {Array.from({ length: 80 }, (_, i) => (
              <div
                key={i}
                style={{
                  borderRadius: "var(--zs-radius-3)",
                  background:
                    i % 2 === 0
                      ? "var(--zs-fill-tertiary)"
                      : "var(--zs-fill-quaternary)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  fontSize: "var(--zs-text-footnote-size)",
                }}
              >
                Cell {i + 1}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-grid-content");
    await expect(root).toHaveAttribute("data-orientation", "both");
  },
};

/* ─── 8. InsideCard — composition with Card ────────────────────────── */
export const InsideCard: Story = {
  name: "Inside Card",
  parameters: {
    docs: {
      description: {
        story:
          "ScrollArea inside a Card content region. A typical compose " +
          "pattern for dashboards: Card frames the panel; ScrollArea " +
          "enhances the overflowing inner content without bleeding " +
          "scroll into the Card chrome.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="InsideCard">
      <Card variant="surface" size="md" style={{ inlineSize: "22rem" }}>
        <Card.Header>
          <Card.Title>Recent activity</Card.Title>
          <Card.Description>Last 40 events</Card.Description>
        </Card.Header>
        <Card.Content>
          <div
            style={{
              blockSize: "12rem",
              borderRadius: "var(--zs-radius-3)",
              border: "0.0625rem solid var(--zs-separator)",
            }}
          >
            <ScrollArea data-testid="scrollarea-inside-card">
              <div style={{ padding: "var(--zs-space-3)" }}>
                {makeListItems(40).map((label) => (
                  <div
                    key={label}
                    style={{
                      paddingBlock: "var(--zs-space-2)",
                      borderBlockEnd:
                        "0.0625rem solid var(--zs-separator)",
                    }}
                  >
                    {label}
                  </div>
                ))}
              </div>
            </ScrollArea>
          </div>
        </Card.Content>
      </Card>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-inside-card");
    await expect(root).toBeInTheDocument();
  },
};

/* ─── 9. RTL — verifies the vertical bar flips to inline-start ─────── */
export const RTL: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Right-to-left direction flips the vertical scrollbar to " +
          "`inset-inline-start: 0` via the `[dir=\"rtl\"]` specificity " +
          "mirror. Horizontal bar's logical anchors handle RTL " +
          "automatically.",
      },
    },
  },
  render: () => (
    <DirectionProvider direction="rtl">
      <div
        className="zs-story-row"
        role="group"
        aria-label="RTL"
        dir="rtl"
        style={{ direction: "rtl" }}
      >
        <div
          style={{
            inlineSize: "20rem",
            blockSize: "12rem",
            border: "0.0625rem solid var(--zs-separator)",
            borderRadius: "var(--zs-radius-5)",
          }}
        >
          <ScrollArea
            type="always"
            data-testid="scrollarea-rtl"
          >
            <div style={{ padding: "var(--zs-space-4)" }}>
              {makeListItems(40).map((label) => (
                <div
                  key={label}
                  style={{
                    paddingBlock: "var(--zs-space-2)",
                    borderBlockEnd:
                      "0.0625rem solid var(--zs-separator)",
                  }}
                >
                  {label}
                </div>
              ))}
            </div>
          </ScrollArea>
        </div>
      </div>
    </DirectionProvider>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const root = canvas.getByTestId("scrollarea-rtl");
    // The Scrollbar mounts after Base UI's overflow observer fires —
    // poll for both the element and its PHYSICAL position.
    await waitFor(() => {
      const bar = root.querySelector(
        '[data-orientation="vertical"].zs-scrollarea__scrollbar',
      );
      expect(bar).not.toBeNull();
      // Under RTL the vertical bar is pinned to inline-start, which is
      // the LEFT physical edge — so the bar's bounding box should sit on
      // the left half of the Root, not the right. Asserting the computed
      // `insetInlineStart === "0px"` would pass even under LTR, where
      // that value is also "0px" while the bar paints on the right.
      // Compare bounding boxes instead to detect the physical edge.
      const rootBox = (root as HTMLElement).getBoundingClientRect();
      const barBox = (bar as HTMLElement).getBoundingClientRect();
      const rootCenterX = rootBox.left + rootBox.width / 2;
      const barCenterX = barBox.left + barBox.width / 2;
      expect(barCenterX).toBeLessThan(rootCenterX);
    });
  },
};

/* ─── 10. KeyboardScroll — arrow-key scroll on focused Viewport ────── */
export const KeyboardScroll: Story = {
  name: "Keyboard scroll",
  parameters: {
    docs: {
      description: {
        story:
          "Native keyboard scroll on the focused Viewport. ArrowDown " +
          "moves scrollTop forward; PageDown jumps a page; End scrolls " +
          "to the bottom. ScrollArea does not intercept these — they " +
          "fire on the real overflow container.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="KeyboardScroll">
      <div
        style={{
          inlineSize: "20rem",
          blockSize: "12rem",
          border: "0.0625rem solid var(--zs-separator)",
          borderRadius: "var(--zs-radius-5)",
        }}
      >
        <ScrollArea.Root data-testid="scrollarea-keyboard-root">
          <ScrollArea.Viewport
            tabIndex={0}
            data-testid="scrollarea-keyboard-viewport"
          >
            <ScrollArea.Content>
              <div style={{ padding: "var(--zs-space-4)" }}>
                {makeListItems(60).map((label) => (
                  <div
                    key={label}
                    style={{
                      paddingBlock: "var(--zs-space-2)",
                      borderBlockEnd:
                        "0.0625rem solid var(--zs-separator)",
                    }}
                  >
                    {label}
                  </div>
                ))}
              </div>
            </ScrollArea.Content>
          </ScrollArea.Viewport>
          <ScrollArea.Scrollbar orientation="vertical">
            <ScrollArea.Thumb />
          </ScrollArea.Scrollbar>
        </ScrollArea.Root>
      </div>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const viewport = canvas.getByTestId(
      "scrollarea-keyboard-viewport",
    ) as HTMLElement;
    viewport.focus();
    const before = viewport.scrollTop;
    await userEvent.keyboard("{End}");
    await waitFor(() => {
      expect(viewport.scrollTop).toBeGreaterThan(before);
    });
  },
};
