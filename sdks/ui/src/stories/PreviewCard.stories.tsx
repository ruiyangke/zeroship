import type { Meta, StoryObj } from "@storybook/react";
import { expect, userEvent, waitFor, within } from "@storybook/test";
import { DirectionProvider } from "@base-ui/react/direction-provider";
import { useMemo } from "react";
import {
  Button,
  PreviewCard,
  createPreviewCardHandle,
} from "../components";

/* Token-driven placeholder thumbnail used in the Basic / LinkPreview
 * stories. Previous revisions used SVG data URIs that contained URL-
 * encoded hex literals which bypassed the literal-hash token-purity
 * grep but still violated the `--zs-*`-only rule. The placeholder now
 * paints with token-driven gradients so the rule holds end-to-end
 * (token-purity hex = 0 inclusive of URL-encoded forms). */
function ThumbnailPlaceholder({
  label,
  tone = "accent",
}: {
  label: string;
  tone?: "accent" | "subtle";
}) {
  const background =
    tone === "accent"
      ? "linear-gradient(135deg, var(--zs-accent), var(--zs-system-orange))"
      : "linear-gradient(135deg, var(--zs-fill-quaternary), var(--zs-fill-tertiary))";
  return (
    <div
      role="img"
      aria-label={label}
      style={{
        inlineSize: "100%",
        blockSize: "6.25rem",
        borderRadius: "var(--zs-radius-2)",
        background,
      }}
    />
  );
}

/* PreviewCard is the rich-hover analog of Tooltip: small intent delay
 * before open, generous close grace so the cursor can cross from the
 * trigger into the popup body. Stories lean on `delay={50}` for the
 * `play()` interactions so the test runner doesn't sit on the 600ms
 * default; visual evidence stories keep the default so the captured
 * PNGs reflect the real-app timing. */

const meta: Meta<typeof PreviewCard> = {
  title: "Components/PreviewCard",
  component: PreviewCard,
  parameters: {
    layout: "fullscreen",
  },
};

export default meta;

type Story = StoryObj<typeof PreviewCard>;

/* ─── 1. Basic — image + title + body ──────────────────────────────── */
export const Basic: Story = {
  name: "Basic (image + title + body)",
  parameters: {
    docs: {
      description: {
        story:
          "Hover the trigger for ~600ms to reveal a rich preview. The " +
          "popup carries a 16rem min-inline-size + 22rem default max so " +
          "consumers compose freely inside. Pointer-leave waits 200ms " +
          "before closing so the cursor can cross into the popup.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Basic">
      <PreviewCard delay={50}>
        <PreviewCard.Trigger
          asChild
          data-testid="previewcard-basic-trigger"
        >
          <Button aria-label="Preview Basic">Hover for preview</Button>
        </PreviewCard.Trigger>
        <PreviewCard.Portal>
          <PreviewCard.Popup data-testid="previewcard-basic-popup">
            <ThumbnailPlaceholder label="Yosemite trip thumbnail" tone="accent" />
            <h3>Yosemite trip notes</h3>
            <p>
              A weekend of granite walls and quiet alpine lakes. Quick read
              before you open the full itinerary.
            </p>
          </PreviewCard.Popup>
        </PreviewCard.Portal>
      </PreviewCard>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /preview basic/i });
    await userEvent.hover(trigger);
    // Wait for the popup testid (portaled into body) — Base UI's
    // intent timer fires asynchronously; findByTestId tolerates the
    // mount delay and ignores text resolution inside the trigger.
    const popup = await body.findByTestId(
      "previewcard-basic-popup",
      undefined,
      { timeout: 3000 },
    );
    await expect(popup).toBeInTheDocument();
    await userEvent.unhover(trigger);
    await waitFor(
      () =>
        expect(
          body.queryByTestId("previewcard-basic-popup"),
        ).not.toBeInTheDocument(),
      { timeout: 2000 },
    );
  },
};

/* ─── 2. UserHandle — avatar + name + bio + follow button ──────────── */
export const UserHandle: Story = {
  name: "User handle preview",
  parameters: {
    docs: {
      description: {
        story:
          "Twitter/X-style profile preview when hovering a username. The " +
          "popup itself can host interactive controls (the Follow button " +
          "remains tabbable while the popup is open).",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="User handle">
      <p>
        Co-authored by{" "}
        <PreviewCard delay={50}>
          <PreviewCard.Trigger
            data-testid="previewcard-userhandle-trigger"
            href="#"
          >
            @ada
          </PreviewCard.Trigger>
          <PreviewCard.Portal>
            <PreviewCard.Popup size="sm" data-testid="previewcard-userhandle-popup">
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "0.75rem",
                }}
              >
                <span
                  aria-hidden="true"
                  style={{
                    inlineSize: "2.5rem",
                    blockSize: "2.5rem",
                    borderRadius: "9999rem",
                    background:
                      "conic-gradient(from 220deg, var(--zs-accent), var(--zs-system-orange))",
                    display: "inline-block",
                  }}
                />
                <div>
                  <h3 style={{ margin: 0 }}>Ada Lovelace</h3>
                  <p style={{ margin: 0 }}>@ada · Mathematician</p>
                </div>
              </div>
              <p>Notes on analytical engines. Posts weekly.</p>
              <Button data-testid="previewcard-userhandle-follow">
                Follow
              </Button>
            </PreviewCard.Popup>
          </PreviewCard.Portal>
        </PreviewCard>
        .
      </p>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByTestId("previewcard-userhandle-trigger");
    await userEvent.hover(trigger);
    // Wait on the popup testid so the assertion never observes the
    // pre-mount DOM. Once mounted, the follow button is the
    // post-condition we actually care about.
    await body.findByTestId(
      "previewcard-userhandle-popup",
      undefined,
      { timeout: 3000 },
    );
    await expect(
      body.getByRole("button", { name: /follow/i }),
    ).toBeInTheDocument();
  },
};

/* ─── 3. LinkPreview — image + headline + url ──────────────────────── */
export const LinkPreview: Story = {
  name: "Link preview (headline + url)",
  parameters: {
    docs: {
      description: {
        story:
          "Wikipedia-style hover preview on an inline link. Default " +
          "trigger renders an <a> so the wrapper stays a real link the " +
          "user can click through.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Link preview">
      <p>
        Learn more about{" "}
        <PreviewCard delay={50}>
          <PreviewCard.Trigger
            data-testid="previewcard-linkpreview-trigger"
            href="https://en.wikipedia.org/wiki/Compio"
          >
            io_uring runtimes
          </PreviewCard.Trigger>
          <PreviewCard.Portal>
            <PreviewCard.Popup data-testid="previewcard-linkpreview-popup">
              <ThumbnailPlaceholder
                label="Article thumbnail"
                tone="subtle"
              />
              <h3>io_uring runtime</h3>
              <p style={{ color: "var(--zs-label-tertiary)" }}>
                en.wikipedia.org
              </p>
              <p>
                Modern asynchronous I/O surface for Linux that lets one
                thread service tens of thousands of connections without
                blocking.
              </p>
            </PreviewCard.Popup>
          </PreviewCard.Portal>
        </PreviewCard>
        .
      </p>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByTestId("previewcard-linkpreview-trigger");
    await userEvent.hover(trigger);
    // Wait for the popup testid (portaled into body) — the trigger text
    // "io_uring runtimes" otherwise resolves a generic text query before
    // the popup mounts.
    await body.findByTestId(
      "previewcard-linkpreview-popup",
      undefined,
      { timeout: 3000 },
    );
    await expect(
      await body.findByText("en.wikipedia.org"),
    ).toBeInTheDocument();
  },
};

/* ─── 4. LongContent — scrollable popup ────────────────────────────── */
export const LongContent: Story = {
  name: "Long content (scrollable popup)",
  parameters: {
    docs: {
      description: {
        story:
          "When the preview content overflows, the popup body scrolls. " +
          "The brief's contract is enforced via a max-block-size on the " +
          "popup inline style — sticky max-inline-size keeps width " +
          "stable.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="Long content">
      <PreviewCard delay={50}>
        <PreviewCard.Trigger
          asChild
          data-testid="previewcard-longcontent-trigger"
        >
          <Button aria-label="Preview LongContent">Release notes</Button>
        </PreviewCard.Trigger>
        <PreviewCard.Portal>
          <PreviewCard.Popup
            size="lg"
            data-testid="previewcard-longcontent-popup"
            style={{ maxBlockSize: "16rem", overflowY: "auto" }}
          >
            <h3>v1.3.0 release notes</h3>
            <p>
              Pre-launch milestone — every API is still fair game to
              break. Notable changes in this drop:
            </p>
            <ul>
              <li>Pre-launch posture documented in AGENTS.md.</li>
              <li>Per-app schema isolation lands for Postgres.</li>
              <li>SQLite parity proves on the same kernel.</li>
              <li>Gateway dispatch picks up CHWBL routing.</li>
              <li>Worker switches to V8 LRU eviction.</li>
              <li>Bundle format documents content-addressed blobs.</li>
              <li>Vite plugin discovers server procedures at build.</li>
              <li>Builder service ships the M0 exit-gate harness.</li>
              <li>Drawer + Toast complete the modal-surface set.</li>
              <li>PreviewCard slice closes Wave 4.</li>
            </ul>
          </PreviewCard.Popup>
        </PreviewCard.Portal>
      </PreviewCard>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", {
      name: /preview longcontent/i,
    });
    await userEvent.hover(trigger);
    // Wait for the popup testid (portaled into body) rather than the
    // trigger text — "Release notes" appears on the trigger too, so a
    // text query would resolve on the trigger before the popup mounts.
    const popup = await body.findByTestId(
      "previewcard-longcontent-popup",
      undefined,
      { timeout: 3000 },
    );
    const overflow = window.getComputedStyle(popup).overflowY;
    await expect(overflow).toMatch(/auto|scroll/);
  },
};

/* ─── 5. AsChild — custom anchor element ───────────────────────────── */
export const AsChild: Story = {
  name: "asChild (custom trigger element)",
  parameters: {
    docs: {
      description: {
        story:
          "asChild routes through the shared Slot helper (Dialog.Close " +
          "pattern, commit 3a64a726) — className, style, refs, and " +
          "event handlers compose. The consumer's <a> stays a real " +
          "link while still driving the hover-open contract.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="AsChild">
      <PreviewCard delay={50}>
        {/* `className="zs-aschild-wrapper-class"` on the wrapper
            Trigger MUST flow through onto the consumer's `<a>` — the
            asChild branch routes the prop through the shared Slot
            helper so the rendered element ends up with both the
            consumer's own className AND the wrapper's. An earlier
            revision destructured `className` but never re-passed it,
            silently dropping it (codex review fix 2). */}
        <PreviewCard.Trigger asChild className="zs-aschild-wrapper-class">
          <a
            data-testid="previewcard-aschild-trigger"
            href="https://example.com/post/42"
            className="zs-aschild-consumer-class"
            style={{ color: "var(--zs-accent)" }}
          >
            Read the announcement post →
          </a>
        </PreviewCard.Trigger>
        <PreviewCard.Portal>
          <PreviewCard.Popup data-testid="previewcard-aschild-popup">
            <h3>Announcement</h3>
            <p>
              The post the trigger refers to. Hovering the inline link
              still opens the preview because asChild forwards the
              hover handlers onto the consumer&apos;s element.
            </p>
          </PreviewCard.Popup>
        </PreviewCard.Portal>
      </PreviewCard>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByTestId("previewcard-aschild-trigger");
    await expect(trigger.tagName.toLowerCase()).toBe("a");
    await expect(trigger).toHaveAttribute("href", "https://example.com/post/42");
    // The wrapper's className flows through the Slot onto the
    // consumer's element alongside the consumer's own className.
    await expect(trigger).toHaveClass("zs-aschild-wrapper-class");
    await expect(trigger).toHaveClass("zs-aschild-consumer-class");
    await userEvent.hover(trigger);
    await body.findByTestId(
      "previewcard-aschild-popup",
      undefined,
      { timeout: 3000 },
    );
  },
};

/* ─── 6. WithArrow ─────────────────────────────────────────────────── */
export const WithArrow: Story = {
  name: "With arrow",
  parameters: {
    docs: {
      description: {
        story:
          "Arrow renders the default 16×8 triangle pointed back at the " +
          "trigger. The wrapper rotates the SVG per `data-side` so the " +
          "apex tracks the anchored side.",
      },
    },
  },
  render: () => (
    <div className="zs-story-row" role="group" aria-label="With arrow">
      <PreviewCard delay={50}>
        <PreviewCard.Trigger
          asChild
          data-testid="previewcard-arrow-trigger"
        >
          <Button aria-label="Preview WithArrow">Open with arrow</Button>
        </PreviewCard.Trigger>
        <PreviewCard.Portal>
          <PreviewCard.Popup data-testid="previewcard-arrow-popup">
            <PreviewCard.Arrow />
            <h3>Anchored preview</h3>
            <p>The arrow points at the trigger so the relationship reads.</p>
          </PreviewCard.Popup>
        </PreviewCard.Portal>
      </PreviewCard>
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /preview witharrow/i });
    await userEvent.hover(trigger);
    const popup = await body.findByTestId(
      "previewcard-arrow-popup",
      undefined,
      { timeout: 3000 },
    );
    await expect(popup.querySelector("svg")).not.toBeNull();
  },
};

/* ─── 7. PlacementSide ─────────────────────────────────────────────── */
export const PlacementSide: Story = {
  name: "Placement side",
  parameters: {
    docs: {
      description: {
        story:
          "Four physical sides arranged so the captured preview lands in " +
          "a readable spot for each. `inline-start` / `inline-end` are " +
          "available as logical aliases (see the RTL story).",
      },
    },
  },
  render: () => (
    <div
      className="zs-story-row"
      role="group"
      aria-label="Placement side"
      style={{
        display: "grid",
        gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
        gap: "2rem",
        padding: "4rem",
      }}
    >
      {(["top", "right", "bottom", "left"] as const).map((side) => (
        <PreviewCard key={side} delay={50}>
          <PreviewCard.Trigger
            asChild
            data-testid={`previewcard-side-${side}-trigger`}
          >
            <Button aria-label={`Preview side ${side}`}>side={side}</Button>
          </PreviewCard.Trigger>
          <PreviewCard.Portal>
            <PreviewCard.Popup side={side} size="sm">
              <PreviewCard.Arrow />
              <h3>side = {side}</h3>
              <p>Anchored on the {side} edge.</p>
            </PreviewCard.Popup>
          </PreviewCard.Portal>
        </PreviewCard>
      ))}
    </div>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByTestId("previewcard-side-top-trigger");
    await userEvent.hover(trigger);
    // Each side popup carries the wrapper class — wait on the
    // generic class hook rather than text content since multiple
    // triggers share the "side = X" pattern.
    await waitFor(
      () =>
        expect(
          canvasElement.ownerDocument.body.querySelector(
            ".zs-preview-card-popup",
          ),
        ).not.toBeNull(),
      { timeout: 3000 },
    );
  },
};

/* ─── 8. RTL ───────────────────────────────────────────────────────── */
export const Rtl: Story = {
  name: "RTL",
  parameters: {
    docs: {
      description: {
        story:
          "Hebrew preview under `direction: rtl`. Logical padding / sizing " +
          "keep the axes correct; `side='inline-start'` resolves to the " +
          "trailing physical side under RTL.",
      },
    },
  },
  render: () => (
    // DirectionProvider seeds Base UI's DirectionContext so the Floating
    // UI positioner resolves `inline-start` / `inline-end` against the
    // RTL axis. A bare `dir="rtl"` div is invisible to Base UI — the
    // popup portals to document.body and the attribute never propagates
    // across that boundary. Slice 11 documented this exact failure mode
    // in `Menu.stories.tsx:566-571`; Slice 19 mirrors it.
    <DirectionProvider direction="rtl">
      <div
        className="zs-story-row"
        role="group"
        aria-label="RTL"
        lang="he"
        style={{ padding: "4rem" }}
      >
        <PreviewCard delay={50}>
          <PreviewCard.Trigger
            asChild
            data-testid="previewcard-rtl-trigger"
          >
            <Button aria-label="Preview RTL">ריחוף לתצוגה</Button>
          </PreviewCard.Trigger>
          <PreviewCard.Portal>
            <PreviewCard.Popup
              side="inline-end"
              size="md"
              data-testid="previewcard-rtl-popup"
            >
              <PreviewCard.Arrow />
              <h3>תצוגה מקדימה</h3>
              <p>
                טקסט בעברית עם פריסה לוגית. הצד מתורגם למיקום הפיזי
                הנכון תחת direction: rtl.
              </p>
            </PreviewCard.Popup>
          </PreviewCard.Portal>
        </PreviewCard>
      </div>
    </DirectionProvider>
  ),
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByRole("button", { name: /preview rtl/i });
    await userEvent.hover(trigger);
    await body.findByTestId(
      "previewcard-rtl-popup",
      undefined,
      { timeout: 3000 },
    );
  },
};

/* ─── 9. DetachedHandle — Root/Trigger paired across the tree ──────── *
 *
 * Imperative-pairing surface (`createHandle()`) for the case where the
 * Trigger renders in a different React subtree from the Root (table
 * row inside one cell, popup mounted from a parent layout, etc.). The
 * story exercises two invariants the wrapper owns:
 *
 *   1. `aria-describedby` on the detached Trigger still references the
 *      Popup id — the augmented handle carries `popupId`.
 *   2. The Root's `delay` / `closeDelay` props still drive timing —
 *      the augmented handle carries those fields too. React context
 *      cannot bridge the gap so the handle is the only carrier.
 *
 * The story sets `delay={0}` and `closeDelay={50}` on the Root so the
 * regression test can observe the popup mounting and unmounting on
 * short timers; the detached-handle pair was historically silently
 * stuck on Base UI's 600ms / 300ms defaults. */
export const DetachedHandle: Story = {
  name: "Detached handle (delay/closeDelay through handle)",
  parameters: {
    docs: {
      description: {
        story:
          "Trigger and Root are paired via `createPreviewCardHandle()` " +
          "across separate subtrees. Root's `delay` / `closeDelay` flow " +
          "through the augmented handle so the detached Trigger honors " +
          "them even though no React context bridges the two sides.",
      },
    },
  },
  render: () => {
    function DetachedRow() {
      // `useMemo` so the handle is stable across renders — recreating
      // it on every render would tear down the pairing on each commit.
      const handle = useMemo(() => createPreviewCardHandle(), []);
      return (
        <div
          className="zs-story-row"
          role="group"
          aria-label="Detached handle"
          style={{
            display: "grid",
            gridTemplateColumns: "1fr 1fr",
            gap: "2rem",
            padding: "2rem",
          }}
        >
          {/* Trigger subtree — no Root, only the imperative handle. */}
          <div data-testid="previewcard-detached-trigger-subtree">
            <PreviewCard.Trigger
              asChild
              handle={handle}
              data-testid="previewcard-detached-trigger"
            >
              <Button aria-label="Preview Detached">
                Hover me (detached)
              </Button>
            </PreviewCard.Trigger>
          </div>
          {/* Root subtree — same handle, separate location in the
              tree. The Root publishes delay=0 + closeDelay=50 so the
              detached-trigger regression test can observe the timing
              actually crossing the handle. */}
          <div data-testid="previewcard-detached-root-subtree">
            <PreviewCard handle={handle} delay={0} closeDelay={50}>
              <PreviewCard.Portal>
                <PreviewCard.Popup
                  data-testid="previewcard-detached-popup"
                >
                  <h3>Detached preview</h3>
                  <p>
                    The Trigger up there shares this Root via{" "}
                    <code>createPreviewCardHandle()</code>. The Root&apos;s
                    timing flows through the handle so the popup opens
                    immediately and closes after 50ms.
                  </p>
                </PreviewCard.Popup>
              </PreviewCard.Portal>
            </PreviewCard>
          </div>
        </div>
      );
    }
    return <DetachedRow />;
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    const trigger = canvas.getByTestId("previewcard-detached-trigger");
    await userEvent.hover(trigger);
    const popup = await body.findByTestId(
      "previewcard-detached-popup",
      undefined,
      { timeout: 1500 },
    );
    await expect(popup).toBeInTheDocument();
    // aria-describedby crosses the handle: the Trigger must reference
    // the Popup's actual mounted id.
    const describedBy = trigger.getAttribute("aria-describedby") ?? "";
    const popupId = popup.id;
    await expect(popupId.length).toBeGreaterThan(0);
    await expect(describedBy.split(/\s+/)).toContain(popupId);
  },
};
